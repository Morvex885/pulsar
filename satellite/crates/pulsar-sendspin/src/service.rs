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
    events: Mutex<Vec<LibEvent>>,
    sink: Sink,
    state_dir: PathBuf,
    tx: UnboundedSender<SendspinEvent>,
    /// false once the owner tears the service down.
    hooks: AtomicBool,
    // Visualizer drain thread only.
    vis_loudness: AtomicU32,
    vis_beat: AtomicBool,
}

impl Listener for Ctx {
    fn event(&self, ev: LibEvent) {
        match sink_request(&ev) {
            Some(SinkRequest::Configure {
                rate,
                channels,
                bits,
            }) => self.sink.configure(rate, channels, bits),
            Some(SinkRequest::Release) => self.sink.release(),
            None => {}
        }
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ev);
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
    /// A Sendspin audio stream is playing on this speaker (a stream end without "stopped" is
    /// reported 1 s late; see `hold`).
    pub fn streaming(&self) -> bool {
        self.streaming.get()
    }

    /// A Sendspin server (e.g. Music Assistant) is connected: the library's own atomic flag.
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

    /// Another local source took over the speaker: leave the Sendspin group (stops our
    /// playback, the rest of the group keeps playing). `streaming` changes only with the
    /// stream end that follows.
    pub fn yield_to_local(&self) {
        if !self.streaming.get() {
            return;
        }
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

    fn stream(&self, changed: Option<bool>) {
        if let Some(active) = changed {
            self.send(SendspinEvent::Stream(active));
        }
    }

    fn take_events(&self) -> Vec<LibEvent> {
        std::mem::take(
            &mut *self
                .ctx
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn event(&mut self, ev: LibEvent) {
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
                    let changed = self.hold.group(playing, &mut self.timers);
                    self.stream(changed);
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
                // The sink was configured inside the callback (Ctx::event).
                let changed = self.hold.start(&mut self.timers);
                self.stream(changed);
            }
            LibEvent::StreamEnd => {
                // The sink was released inside the callback (Ctx::event).
                let changed = self.hold.end(&mut self.timers, self.hooks);
                self.stream(changed);
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
                        let changed = self.core.hold.finish(&mut self.core.timers);
                        self.core.stream(changed);
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
        for ev in self.core.take_events() {
            self.core.event(ev);
        }
        let c = self.client.is_connected();
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
            let changed = self.core.hold.finish(&mut self.core.timers);
            self.core.stream(changed);
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
        for ev in core.take_events() {
            core.event(ev);
        }
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
        ctx.event(LibEvent::StreamStart(Some(StreamParams {
            codec: 0,
            rate: 48000,
            channels: 2,
            bits: 16,
        })));
        assert!(ctx.sink.accepts_audio());
        ctx.event(LibEvent::StreamEnd);
        assert!(!ctx.sink.accepts_audio());
        // Incomplete parameters leave the sink alone, as in C++.
        ctx.event(LibEvent::StreamStart(None));
        assert!(!ctx.sink.accepts_audio());
        assert_eq!(ctx.events.lock().unwrap().len(), 3);
    }
}
