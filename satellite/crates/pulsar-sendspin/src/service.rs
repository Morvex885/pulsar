//! The Sendspin player service: one sendspin-cpp client in server-initiated mode (the library
//! listens on ws://<ip>:<port>/sendspin; the controller advertises `_sendspin._tcp` over mDNS).
//!
//! Threads: the library's protocol task, the player's sync (decode) task and the visualizer's
//! drain thread are its own; its main loop (`SendspinClient::loop()`) runs on the runtime thread
//! from the actor's 10 ms tick, so every main loop callback is handled by the actor right after
//! the call that delivered it. Decoded PCM goes from the sync task straight to the ALSA sink;
//! visualizer frames go from the drain thread straight to the event channel.

use std::cell::Cell;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex, PoisonError};

use log::{debug, error, info, warn};
use pulsar_core::{MusicFrame, Timers, actors};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::event::{LibEvent, SinkRequest, sink_request};
use crate::ffi::{self, Client, Codec, Command, Config, Format, Listener, Scale, VisType};
use crate::hold::{Key, StreamHold};
use crate::sink::Sink;
use crate::{SendspinConfig, SendspinEvent, Transport, music, persist, volume_percent};

/// SendspinClient::loop() cadence (sendspin-cpp integration guide).
const TICK_MS: u64 = 10;

/// Player formats, preferred first.
const FORMATS: [Format; 5] = [
    Format {
        codec: Codec::Flac,
        channels: 2,
        rate: 48000,
        bits: 16,
    },
    Format {
        codec: Codec::Flac,
        channels: 2,
        rate: 44100,
        bits: 16,
    },
    Format {
        codec: Codec::Opus,
        channels: 2,
        rate: 48000,
        bits: 16,
    },
    Format {
        codec: Codec::Pcm,
        channels: 2,
        rate: 48000,
        bits: 16,
    },
    Format {
        codec: Codec::Pcm,
        channels: 2,
        rate: 44100,
        bits: 16,
    },
];

const VIS_TYPES: [VisType; 3] = [VisType::Spectrum, VisType::Loudness, VisType::Beat];

type SsClient = Client<Ctx>;

/// State the library's threads reach through the listener.
struct Ctx {
    /// Main loop events, delivered inside `Client::run_loop` (or start / drop) and handled by
    /// the actor right after it.
    events: Mutex<Vec<(LibEvent, Option<u64>)>>,
    sink: Sink,
    state_dir: PathBuf,
    tx: UnboundedSender<SendspinEvent>,
    /// false once the owner tears the service down.
    hooks: AtomicBool,
    // Visualizer drain thread only.
    vis_loudness: AtomicU32,
    vis_beat: AtomicBool,
}

impl Ctx {
    fn revoke_before(self: &Arc<Self>, streaming: &Cell<bool>, order: u64) -> bool {
        if !self.sink.revoke_before(order) {
            return false;
        }
        // Если sync task уже перестал писать, закрытие PCM всё равно должно выполниться.
        let ctx = self.clone();
        tokio::task::spawn_blocking(move || ctx.sink.finish_request());
        // Не ждём StreamEnd от сервера: допуск к выходу уже закрыт.
        self.stop_reporting(streaming);
        true
    }

    fn cancel_local(self: &Arc<Self>, streaming: &Cell<bool>) -> bool {
        // Явная остановка отменяет и старт без записанных кадров, и удержанный индикатор.
        self.revoke_before(streaming, pulsar_core::audio_start_order())
            || self.stop_reporting(streaming)
    }

    fn stop_reporting(&self, streaming: &Cell<bool>) -> bool {
        if !streaming.replace(false) {
            return false;
        }
        let _ = self.tx.send(SendspinEvent::Stream(None));
        true
    }
}

impl Listener for Ctx {
    fn event(&self, ev: LibEvent) {
        let order = match sink_request(&ev) {
            Some(SinkRequest::Configure {
                rate,
                channels,
                bits,
            }) => Some(self.sink.configure(rate, channels, bits)),
            Some(SinkRequest::Release) => {
                self.sink.release();
                None
            }
            None => None,
        };
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((ev, order));
    }

    fn load_blob(&self, key: &OsStr) -> Option<Vec<u8>> {
        persist::load_blob(&self.state_dir, key)
    }

    fn save_blob(&self, key: &OsStr, data: &[u8]) -> bool {
        persist::save_blob(&self.state_dir, key, data)
    }

    fn audio_write(&self, data: &[u8], timeout_ms: u32) -> (usize, u32, i64) {
        self.sink.write(data, timeout_ms)
    }

    fn loudness(&self, v: u16) {
        self.vis_loudness
            .store(music::loudness(v).to_bits(), Relaxed);
    }

    fn beat(&self) {
        self.vis_beat.store(true, Relaxed);
    }

    fn spectrum(&self, bins: &[u16]) {
        let Some(bands) = music::bands(bins) else {
            return;
        };
        let frame = MusicFrame {
            bands,
            loudness: f32::from_bits(self.vis_loudness.load(Relaxed)),
            beat: self.vis_beat.swap(false, Relaxed),
        };
        if self.hooks.load(Relaxed) {
            let _ = self.tx.send(SendspinEvent::Music(frame));
        }
    }
}

enum Cmd {
    ReportVolume { volume: f32, muted: bool },
    ConfirmPairingWindow,
}

/// Lives on the runtime thread (`!Send`). The actor stops (goodbye to the server, library
/// threads joined) once every handle is dropped.
#[derive(Clone)]
pub struct SendspinHandle {
    client: Rc<SsClient>,
    ctx: Arc<Ctx>,
    streaming: Rc<Cell<bool>>,
    cmd: UnboundedSender<Cmd>,
}

impl SendspinHandle {
    /// На этом динамике играет аудиопоток Sendspin (конец потока без «stopped» сообщается на 1 с
    /// позже; см. `hold`). Новый поток учитывается после первой успешной записи кадров в PCM.
    pub fn streaming(&self) -> bool {
        self.streaming.get()
    }

    /// Проверяет, что записавший кадры PCM ещё принадлежит этому запуску.
    pub fn stream_is_current(&self, order: u64) -> bool {
        self.streaming.get() && self.ctx.sink.output_is_current(order)
    }

    /// Sendspin-сервер (например, Music Assistant) подключён: атомарный флаг, который пишет поток
    /// библиотеки; чтение (acquire) не блокирует.
    pub fn connected(&self) -> bool {
        self.client.is_connected()
    }

    /// Software gain 0..1 applied to Sendspin audio (ramped), used for ducking.
    pub fn set_duck_gain(&self, g: f32) {
        self.ctx.sink.set_gain(g);
    }

    /// Local volume / mute change (buttons, HA): reported to the server when it differs.
    pub fn report_volume(&self, volume: f32, muted: bool) {
        let _ = self.cmd.send(Cmd::ReportVolume { volume, muted });
    }

    /// Group transport command to the server (controller role), for the whole Sendspin group.
    /// false when no server is connected or it does not offer the command now (e.g. NEXT with
    /// an empty queue).
    pub fn transport(&self, t: Transport) -> bool {
        if !self.client.is_connected() {
            return false;
        }
        let (c, name) = match t {
            Transport::Play => (Command::Play, "play"),
            Transport::Pause => (Command::Pause, "pause"),
            Transport::Stop => (Command::Stop, "stop"),
            Transport::Next => (Command::Next, "next"),
            Transport::Previous => (Command::Previous, "previous"),
        };
        let ok = self.client.send_command(c);
        info!(
            "sendspin: transport {name} {}",
            if ok {
                "sent"
            } else {
                "not offered by the server"
            }
        );
        ok
    }

    /// Явная команда HA освобождает локальный выход и выводит колонку из группы, не
    /// останавливая остальные колонки. Отменяет даже ещё не начавшуюся запись.
    pub fn yield_to_local(&self) {
        if self.ctx.cancel_local(&self.streaming) {
            self.leave_group();
        }
    }

    /// Передаёт выход более свежему запуску Music, не ставя остальные плееры группы на паузу.
    pub fn yield_to_music(&self, order: u64) {
        if self.ctx.revoke_before(&self.streaming, order) {
            self.leave_group();
        }
    }

    /// Bluetooth перехватил динамик. Pause касается всей группы, но не подтверждает
    /// остановку локального PCM: закрыть его независимо от ответа сервера.
    pub fn pause_for_bluetooth(&self, order: u64) {
        if self.ctx.revoke_before(&self.streaming, order) && !self.transport(Transport::Pause) {
            self.leave_group();
        }
    }

    fn leave_group(&self) {
        info!("sendspin: local playback took over, leaving the group");
        self.client.leave();
    }

    /// The operator performed the pairing-window gesture.
    pub fn confirm_pairing_window(&self) {
        let _ = self.cmd.send(Cmd::ConfirmPairingWindow);
    }
}

/// Starts the client (WebSocket server on `cfg.port`) with the current local volume / mute.
/// Must run inside the controller's `LocalSet`.
pub fn spawn(
    cfg: SendspinConfig,
    volume: f32,
    muted: bool,
    master_gain: f32,
) -> Result<(SendspinHandle, UnboundedReceiver<SendspinEvent>), String> {
    ffi::set_log_level_info();
    if !persist::ensure_dir(&cfg.state_dir) {
        warn!("sendspin: identity will not persist");
    }
    let unpaired = persist::unpaired_access(&cfg.state_dir);
    let (tx, rx) = unbounded_channel();
    let initial_gain = if muted { 0.0 } else { master_gain };
    let ctx = Arc::new(Ctx {
        events: Mutex::new(Vec::new()),
        sink: Sink::new(cfg.model),
        state_dir: cfg.state_dir.clone(),
        tx,
        hooks: AtomicBool::new(true),
        vis_loudness: AtomicU32::new(0),
        vis_beat: AtomicBool::new(false),
    });
    ctx.sink.set_gain(initial_gain);
    let config = Config {
        name: &cfg.name,
        product_name: &cfg.product_name,
        manufacturer: &cfg.manufacturer,
        software_version: &cfg.software_version,
        mac: &cfg.mac,
        port: cfg.port,
        formats: &FORMATS,
        // 8 log-spaced bands over the range the speaker-reference analyzer covers; ~30 frames/s
        // matches the LED frame period (33 ms). Only ~1/7 of buffer_capacity holds wire data and
        // the server sends frames as far ahead as audio (seconds): 64 KB keeps its flow control
        // from starving them.
        visualizer: ffi::Visualizer {
            buffer_capacity: 65536,
            types: &VIS_TYPES,
            rate_max: 30,
            bins: 8,
            scale: Scale::Log,
            f_min: 40,
            f_max: 7600,
            display_offset_ms: 0,
        },
        // Dynamic pairing code: six digits shown on the 4-digit clock (paged as 3-3 groups).
        pairing_code_display_digits: true,
        // The pairing token is printed to the daemon log.
        pairing_psk_location: "operator",
        unpaired_access: unpaired,
    };
    let started = Client::new(&config, ctx.clone()).filter(|c| match c.start() {
        Some(ok) => ok,
        None => {
            error!("sendspin: client start threw an exception");
            false
        }
    });
    let Some(client) = started else {
        error!("sendspin: client start failed");
        return Err("sendspin: client start failed".into());
    };
    client.set_output_delay_adjustable(true);
    client.update_volume(volume_percent(volume));
    client.update_muted(muted);
    info!(
        "sendspin: listening on ws://0.0.0.0:{}/sendspin as '{}', client_id {}, unpaired access {}",
        cfg.port,
        cfg.name,
        client.client_id(),
        if unpaired { "on" } else { "off" }
    );
    if let Some(tok) = client.pairing_token() {
        info!("sendspin: pairing token {tok}");
    }

    let client = Rc::new(client);
    let streaming = Rc::new(Cell::new(false));
    let (cmd, cmds) = unbounded_channel();
    let actor = Actor {
        client: client.clone(),
        core: Core {
            ctx: ctx.clone(),
            hold: StreamHold::new(streaming.clone()),
            timers: Timers::new(),
            hooks: true,
        },
        connected: false,
        loop_failed_logged: false,
    };
    actors::spawn_actor("sendspin", actor.run(cmds));
    Ok((
        SendspinHandle {
            client,
            ctx,
            streaming,
            cmd,
        },
        rx,
    ))
}

/// Main loop event handling, also used after the client is gone (teardown).
struct Core {
    ctx: Arc<Ctx>,
    hold: StreamHold,
    timers: Timers<Key>,
    /// false while the owner tears the service down: no events, no held stream end.
    hooks: bool,
}

impl Core {
    fn send(&self, ev: SendspinEvent) {
        if self.hooks {
            let _ = self.ctx.tx.send(ev);
        }
    }

    fn stream_ended(&self, ended: bool) {
        if ended {
            self.send(SendspinEvent::Stream(None));
        }
    }

    fn output_ready(&mut self) {
        if let Some(order) = self.ctx.sink.take_ready_start() {
            self.hold.ready(&mut self.timers);
            self.send(SendspinEvent::Stream(Some(order)));
        }
    }

    fn dispatch_events(&mut self) {
        let events = std::mem::take(
            &mut *self
                .ctx
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for ev in events {
            self.event(ev);
        }
    }

    fn event(&mut self, (ev, order): (LibEvent, Option<u64>)) {
        match ev {
            LibEvent::Group { playing, name } => {
                info!(
                    "sendspin: group {}{}{}",
                    name.as_deref().unwrap_or("-"),
                    if playing.is_some() { " state=" } else { "" },
                    match playing {
                        Some(true) => "playing",
                        Some(false) => "stopped",
                        None => "",
                    }
                );
                if let Some(playing) = playing {
                    let ended = self.hold.group(playing, &mut self.timers);
                    self.stream_ended(ended);
                }
            }
            LibEvent::TimeSync(error) => debug!("sendspin: time sync error {error:.0} us"),
            LibEvent::PairingStarted(id) => info!("sendspin: pairing started by server {id}"),
            LibEvent::PairingSucceeded(id) => info!("sendspin: paired with server {id}"),
            LibEvent::PairingFailed { server_id, reason } => warn!(
                "sendspin: pairing with {server_id} failed: {}",
                abort_reason(reason)
            ),
            LibEvent::Trust { paired } => info!(
                "sendspin: connection trust {}",
                if paired { "paired" } else { "unpaired" }
            ),
            LibEvent::PairingCode(code) => {
                info!(
                    "sendspin: PAIRING CODE {}-{} (enter it in Music Assistant)",
                    code.get(..3).unwrap_or(&code),
                    code.get(3..).unwrap_or("")
                );
                self.send(SendspinEvent::PairingCode(code));
            }
            LibEvent::PairingCodeCleared => {
                info!("sendspin: pairing code withdrawn");
                self.send(SendspinEvent::PairingCode(String::new()));
            }
            LibEvent::PairingWindow(open) => {
                if open {
                    info!(
                        "sendspin: pairing needs confirmation on the device: press the action button"
                    );
                }
                self.send(SendspinEvent::PairingWindow(open));
            }
            LibEvent::StreamStart(None) => {
                warn!("sendspin: stream start without complete parameters");
            }
            LibEvent::StreamStart(Some(p)) => {
                info!(
                    "sendspin: stream start {} {} Hz/{} ch/{} bit",
                    codec_name(p.codec),
                    p.rate,
                    p.channels,
                    p.bits
                );
                // Следующий колбэк мог уже остановить или заменить этот PCM.
                if order.is_some_and(|order| self.ctx.sink.owns_start(order)) {
                    self.hold.starting();
                }
            }
            LibEvent::StreamEnd => {
                // The sink was released inside the callback (Ctx::event).
                let ended = self.hold.end(&mut self.timers, self.hooks);
                self.stream_ended(ended);
            }
            LibEvent::Volume(v) => {
                info!("sendspin: server volume {v}");
                self.send(SendspinEvent::Volume(f32::from(v) / 100.0));
            }
            LibEvent::Mute(m) => {
                info!("sendspin: server mute {}", u8::from(m));
                self.send(SendspinEvent::Mute(m));
            }
            LibEvent::OutputDelay(ms) => info!("sendspin: output delay {ms} ms"),
            LibEvent::ControllerState {
                commands,
                volume,
                muted,
            } => debug!(
                "sendspin: controller state: {commands} commands, group volume {volume}, muted {}",
                u8::from(muted)
            ),
            LibEvent::Metadata { artist, title } => {
                if artist.is_some() || title.is_some() {
                    info!(
                        "sendspin: now playing: {} - {}",
                        artist.as_deref().unwrap_or("?"),
                        title.as_deref().unwrap_or("?")
                    );
                }
            }
            LibEvent::VisualizerStart { types, rate_max } => {
                info!("sendspin: visualizer stream start, {types} types, {rate_max} fps");
            }
            LibEvent::VisualizerEnd => info!("sendspin: visualizer stream end"),
        }
    }
}

struct Actor {
    client: Rc<SsClient>,
    core: Core,
    connected: bool,
    loop_failed_logged: bool,
}

impl Actor {
    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>) {
        self.core.timers.set_repeating(Key::Tick, TICK_MS, TICK_MS);
        loop {
            tokio::select! {
                biased;
                key = self.core.timers.next() => match key {
                    Key::Tick => self.poll(),
                    Key::StreamEnd => {
                        let ended = self.core.hold.finish(&mut self.core.timers);
                        self.core.stream_ended(ended);
                    }
                },
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => break,
                },
            }
        }
        self.teardown();
    }

    fn poll(&mut self) {
        if !self.client.run_loop() && !self.loop_failed_logged {
            self.loop_failed_logged = true;
            error!("sendspin: client loop threw an exception");
        }
        self.core.dispatch_events();
        let c = self.client.is_connected();
        if c {
            // Проверяем результат записи задачи синхронизации после колбэков этого тика.
            self.core.output_ready();
        }
        if c == self.connected {
            return;
        }
        self.connected = c;
        if c {
            let info = self.client.server_info();
            info!(
                "sendspin: server connected: {} ({})",
                info.as_ref().map_or("?", |(name, _)| name.as_str()),
                info.as_ref().map_or("?".into(), |(_, id)| hex_tail(id, 12))
            );
        } else {
            info!("sendspin: server disconnected");
            let ended = self.core.hold.finish(&mut self.core.timers);
            self.core.stream_ended(ended);
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::ReportVolume { volume, muted } => {
                let v = volume_percent(volume);
                if v != self.client.volume() {
                    self.client.update_volume(v);
                }
                if muted != self.client.muted() {
                    self.client.update_muted(muted);
                }
            }
            Cmd::ConfirmPairingWindow => {
                info!("sendspin: pairing window confirmed by the operator");
                self.client.confirm_pairing_window();
            }
        }
    }

    /// The owner is gone: goodbye to the server and the library's threads joined (inside the
    /// client's drop); its last callbacks (stream end, withdrawn pairing prompts) still release
    /// the sink and log, without events.
    fn teardown(self) {
        let Actor {
            client, mut core, ..
        } = self;
        core.hooks = false;
        core.ctx.hooks.store(false, Relaxed);
        drop(client);
        core.dispatch_events();
    }
}

/// The first `n` characters of an id, "…" if cut.
fn hex_tail(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

fn codec_name(codec: u8) -> &'static str {
    ["flac", "opus", "pcm", "?"][usize::from(codec.min(3))]
}

/// sendspin::SendspinPairAbortReason.
fn abort_reason(r: u8) -> &'static str {
    match r {
        0 => "attempt_timeout",
        1 => "concurrent_attempt",
        2 => "method_not_supported",
        3 => "pairing_code_mismatch",
        4 => "user_cancelled",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StreamParams;

    const PARAMS: StreamParams = StreamParams {
        codec: 0,
        rate: 48000,
        channels: 2,
        bits: 16,
    };

    fn ctx() -> (Ctx, UnboundedReceiver<SendspinEvent>) {
        let (tx, rx) = unbounded_channel();
        let ctx = Ctx {
            events: Mutex::new(Vec::new()),
            sink: Sink::new(pulsar_core::model::model_from_dt_id(b"axg_s420_v03").unwrap()),
            state_dir: PathBuf::from("/nonexistent"),
            tx,
            hooks: AtomicBool::new(true),
            vis_loudness: AtomicU32::new(0),
            vis_beat: AtomicBool::new(false),
        };
        (ctx, rx)
    }

    // The library's sync task writes its priming silence right after on_stream_start returns,
    // before the actor runs: the sink must take it by then.
    #[test]
    fn stream_callbacks_drive_the_sink_before_the_actor_runs() {
        let (ctx, _rx) = ctx();
        assert!(!ctx.sink.accepts_audio());
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        assert!(ctx.sink.accepts_audio());
        ctx.event(LibEvent::StreamEnd);
        assert!(!ctx.sink.accepts_audio());
        // Incomplete parameters leave the sink alone, as in C++.
        ctx.event(LibEvent::StreamStart(None));
        assert!(!ctx.sink.accepts_audio());
        assert_eq!(ctx.events.lock().unwrap().len(), 3);
    }

    fn rig() -> (Core, Rc<Cell<bool>>, UnboundedReceiver<SendspinEvent>) {
        let (ctx, rx) = ctx();
        let streaming = Rc::new(Cell::new(false));
        let core = Core {
            ctx: Arc::new(ctx),
            hold: StreamHold::new(streaming.clone()),
            timers: Timers::new(),
            hooks: true,
        };
        (core, streaming, rx)
    }

    fn dispatch(core: &mut Core) {
        core.dispatch_events();
        core.output_ready();
    }

    fn recv_start(rx: &mut UnboundedReceiver<SendspinEvent>) -> u64 {
        let Ok(SendspinEvent::Stream(Some(order))) = rx.try_recv() else {
            panic!("expected a stream start");
        };
        order
    }

    #[tokio::test]
    async fn newer_music_revokes_sendspin_before_or_after_actor_dispatch() {
        for dispatch_first in [false, true] {
            let (mut core, streaming, mut rx) = rig();
            let ctx = core.ctx.clone();
            ctx.event(LibEvent::StreamStart(Some(PARAMS)));
            ctx.sink.test_write_result(1, false);
            assert!(ctx.sink.accepts_audio());
            // Другой аудиопоток может стартовать, пока событие Sendspin ещё в очереди.
            let music = pulsar_core::audio_start_order();
            if dispatch_first {
                dispatch(&mut core);
                let sendspin = recv_start(&mut rx);
                assert!(sendspin < music);
                assert!(ctx.sink.owns_start(sendspin));
            }

            assert!(ctx.revoke_before(&streaming, music));
            assert!(!streaming.get());
            assert_eq!(ctx.audio_write(&[1; 4], 0), (4, 0, 0));
            assert!(!ctx.sink.accepts_audio());
            if dispatch_first {
                assert_eq!(rx.try_recv(), Ok(SendspinEvent::Stream(None)));
            } else {
                // Обработка очереди не должна вернуть уже вытесненное поколение.
                dispatch(&mut core);
                assert!(rx.try_recv().is_err());
            }
        }
    }

    #[test]
    fn newer_callback_survives_older_music_during_held_restart() {
        let (mut core, streaming, mut rx) = rig();
        let ctx = core.ctx.clone();
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        let first = recv_start(&mut rx);

        ctx.event(LibEvent::StreamEnd);
        dispatch(&mut core);
        assert!(streaming.get()); // Ждём возобновления после перемотки или смены трека.
        assert!(!ctx.sink.owns_start(first));
        let music = pulsar_core::audio_start_order();
        assert!(!ctx.revoke_before(&streaming, music)); // Выход уже свободен.
        assert!(rx.try_recv().is_err());

        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        assert!(!ctx.revoke_before(&streaming, music)); // Новый старт ещё в очереди.
        assert!(ctx.sink.accepts_audio());
        dispatch(&mut core);
        assert!(rx.try_recv().is_err()); // Без записанных кадров выход ещё не захвачен.
        assert!(streaming.get()); // Но прежний индикатор пока удерживается.
        assert!(core.timers.is_set(Key::StreamEnd));
        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        let second = recv_start(&mut rx);
        assert!(second > music);
        assert!(ctx.sink.owns_start(second));
        assert!(streaming.get());
        assert!(rx.try_recv().is_err()); // Индикатор не переключался на паузу.
        assert!(!core.timers.is_set(Key::StreamEnd));
    }

    #[tokio::test]
    async fn bluetooth_revokes_sendspin_until_a_new_stream_starts() {
        let (mut core, streaming, mut rx) = rig();
        let ctx = core.ctx.clone();
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        let first = recv_start(&mut rx);
        assert!(ctx.sink.accepts_audio());

        assert!(ctx.revoke_before(&streaming, pulsar_core::audio_start_order()));
        assert_eq!(rx.try_recv(), Ok(SendspinEvent::Stream(None)));
        assert!(!streaming.get());
        assert!(!ctx.sink.accepts_audio());
        assert_eq!(ctx.audio_write(&[1; 4], 0), (4, 0, 0));

        ctx.event(LibEvent::Group {
            playing: Some(true),
            name: None,
        });
        ctx.event(LibEvent::StreamStart(None));
        dispatch(&mut core);
        assert!(!ctx.sink.accepts_audio());
        assert!(rx.try_recv().is_err());

        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        let second = recv_start(&mut rx);
        assert!(second > first);
        assert!(streaming.get());
        assert!(ctx.sink.accepts_audio());
    }

    #[test]
    fn failed_or_zero_write_does_not_start_playback_and_retry_keeps_callback_order() {
        let (mut core, streaming, mut rx) = rig();
        let ctx = core.ctx.clone();
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        let later = pulsar_core::audio_start_order();
        dispatch(&mut core);
        assert!(!streaming.get());
        assert!(rx.try_recv().is_err());

        for lost in [true, false] {
            ctx.sink.test_write_result(0, lost);
            dispatch(&mut core);
            assert!(!streaming.get());
            assert!(rx.try_recv().is_err());
        }

        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        let sendspin = recv_start(&mut rx);
        assert!(sendspin < later);
        assert!(ctx.sink.output_is_current(sendspin));
        assert!(streaming.get());
        dispatch(&mut core);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn explicit_local_cancel_discards_pending_retries_and_unpublished_starts() {
        for (written, lost) in [(0, true), (0, false), (1, false)] {
            let (mut core, streaming, mut rx) = rig();
            let ctx = core.ctx.clone();
            ctx.event(LibEvent::StreamStart(Some(PARAMS)));
            ctx.sink.test_write_result(written, lost);
            assert!(!streaming.get());

            assert!(ctx.cancel_local(&streaming));
            assert!(!ctx.cancel_local(&streaming));
            assert_eq!(ctx.audio_write(&[1; 4], 0), (4, 0, 0));
            dispatch(&mut core);
            assert!(!ctx.sink.accepts_audio());
            assert!(!streaming.get());
            assert!(rx.try_recv().is_err());

            ctx.event(LibEvent::StreamStart(Some(PARAMS)));
            ctx.sink.test_write_result(1, false);
            dispatch(&mut core);
            recv_start(&mut rx);
            assert!(streaming.get());
        }
    }

    #[test]
    fn ended_or_reconfigured_output_is_not_published_from_the_actor_queue() {
        let (mut core, streaming, mut rx) = rig();
        let ctx = core.ctx.clone();
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        ctx.sink.test_write_result(1, false);
        ctx.event(LibEvent::StreamEnd);
        dispatch(&mut core);
        assert!(!streaming.get());
        assert!(rx.try_recv().is_err());

        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        ctx.sink.test_write_result(1, false);
        let between = pulsar_core::audio_start_order();
        ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        dispatch(&mut core);
        assert!(!streaming.get());
        assert!(rx.try_recv().is_err());

        ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        assert!(recv_start(&mut rx) > between);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_restart_does_not_cancel_held_end() {
        let (mut core, streaming, mut rx) = rig();
        core.ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        core.ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        recv_start(&mut rx);

        core.ctx.event(LibEvent::StreamEnd);
        dispatch(&mut core);
        assert!(streaming.get());
        core.ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        core.ctx.sink.test_write_result(0, true);
        dispatch(&mut core);
        assert!(streaming.get());
        assert!(rx.try_recv().is_err());

        assert_eq!(core.timers.next().await, Key::StreamEnd);
        let ended = core.hold.finish(&mut core.timers);
        core.stream_ended(ended);
        assert_eq!(rx.try_recv(), Ok(SendspinEvent::Stream(None)));
        assert!(!streaming.get());
    }

    #[test]
    fn stopped_group_while_opening_still_ends_without_a_hold() {
        let (mut core, streaming, mut rx) = rig();
        core.ctx.event(LibEvent::StreamStart(Some(PARAMS)));
        dispatch(&mut core);
        core.ctx.event(LibEvent::Group {
            playing: Some(false),
            name: None,
        });
        core.ctx.sink.test_write_result(1, false);
        dispatch(&mut core);
        recv_start(&mut rx);

        core.ctx.event(LibEvent::StreamEnd);
        dispatch(&mut core);
        assert_eq!(rx.try_recv(), Ok(SendspinEvent::Stream(None)));
        assert!(!streaming.get());
    }
}
