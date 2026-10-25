//! Player/Mixer probe. Run with the daemon stopped.
//! usage: player_probe <music_uri> <announce_file> <http_base> [https_url]
//!  0. Mixer: set_volume(v) / mute / unmute with v = min(current volume, 0.2), each read back
//!     with amixer; playback below runs at v (0.2 = raw 82 on the stock curve, the port's test
//!     cap). Nothing plays while the master is muted (raw 0). The original raw values are
//!     written back at the end, on SIGINT / SIGTERM / SIGHUP and on a panic.
//!  1. MUSIC plays <music_uri> (gain 0.3)
//!  2. at +1.5 s ANNOUNCE plays [<announce_file>, <http_base>/announce.flac] (gain 0.3) with
//!     MUSIC ducked to 0.08; restored when the announcement is done
//!  3. MUSIC paused for 1 s and resumed
//!  4. after MUSIC done: MUSIC plays <http_base>/tone.mp3, then <http_base>/missing.flac (expect
//!     done ok=0), then [https_url] for 5 s (state must reach PLAYING; then stop())
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::process::{Command, ExitCode};
use std::sync::Mutex;

use pulsar_core::Timers;
use pulsar_media::{Channel, Mixer, PlayId, PlayState, PlayerEvent, mixer};
use tokio::signal::unix::{SignalKind, signal};

const MASTER: &str = "Master Playback Volume";
/// Mixer scale: the port's cap for test sounds.
const MAX_TEST_VOLUME: f32 = 0.2;

/// The control and its original raw values, written back once (normal exit, signal, panic).
static RESTORE: Mutex<Option<(pulsar_alsa::Ctl, [i64; 2])>> = Mutex::new(None);

fn restore() {
    // try_lock: a panic while the lock is held must not deadlock the hook; a poisoned lock
    // still holds the values to write back.
    let mut g = match RESTORE.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    if let Some((ctl, raw)) = g.take() {
        match ctl.write_integers(MASTER, &raw) {
            Ok(()) => println!("mixer restored raw={raw:?}"),
            Err(rc) => println!(
                "mixer restore raw={raw:?} failed: {}",
                pulsar_alsa::strerror(rc)
            ),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Announce,
    Pause,
    Resume,
    HttpsStop,
    ExitAfterDone,
    ExitAfterStop,
    Timeout,
}

fn sname(s: PlayState) -> &'static str {
    match s {
        PlayState::Idle => "IDLE",
        PlayState::Loading => "LOADING",
        PlayState::Playing => "PLAYING",
        PlayState::Paused => "PAUSED",
        PlayState::Error => "ERROR",
    }
}

fn amixer() {
    let rc = Command::new("sh")
        .arg("-c")
        .arg(format!("amixer -c0 cget name='{MASTER}' | grep ': values'"))
        .status();
    if let Err(e) = rc {
        println!("amixer: {e}");
    }
}

/// Both raw values of the master control, read past the Mixer (which keeps no raw value).
fn read_raw(ctl: &pulsar_alsa::Ctl) -> Option<[i64; 2]> {
    Some([
        ctl.read_integer(MASTER, 0).ok()?,
        ctl.read_integer(MASTER, 1).ok()?,
    ])
}

struct Probe {
    t0: i64,
    music: String,
    announce: String,
    http: String,
    https: String,
    player: pulsar_media::PlayerHandle,
    timers: Timers<Key>,
    music_id: Option<PlayId>,
    announce_id: Option<PlayId>,
    tone_id: Option<PlayId>,
    missing_id: Option<PlayId>,
    https_id: Option<PlayId>,
    saw_state: bool,
}

enum Step {
    Continue,
    Exit,
    InitFailed,
    Signal,
}

impl Probe {
    fn ts(&self) -> f64 {
        (pulsar_core::now_ms() - self.t0) as f64 / 1000.0
    }

    fn start(&mut self) {
        self.player.set_channel_gain(Channel::Music, 0.3);
        self.player.set_channel_gain(Channel::Announce, 0.3);
        println!("[{:6.2}] == MUSIC {}", self.ts(), self.music);
        self.music_id = Some(self.player.play(Channel::Music, vec![self.music.clone()]));
        self.timers.set(Key::Announce, 1500);
        self.timers.set(Key::Timeout, 60_000);
    }

    fn phase_http(&mut self) {
        println!("[{:6.2}] == HTTP {}/tone.mp3", self.ts(), self.http);
        let url = format!("{}/tone.mp3", self.http);
        self.tone_id = Some(self.player.play(Channel::Music, vec![url]));
    }

    fn phase_https(&mut self) {
        if self.https.is_empty() {
            self.timers.set(Key::ExitAfterDone, 500);
            return;
        }
        println!("[{:6.2}] == HTTPS {}", self.ts(), self.https);
        self.https_id = Some(self.player.play(Channel::Music, vec![self.https.clone()]));
        self.timers.set(Key::HttpsStop, 5000);
    }

    fn timer(&mut self, key: Key) -> Step {
        match key {
            Key::Announce => {
                println!(
                    "[{:6.2}] == ANNOUNCE [{}, {}/announce.flac], duck music",
                    self.ts(),
                    self.announce,
                    self.http
                );
                self.player.set_channel_gain(Channel::Music, 0.08);
                let urls = vec![
                    self.announce.clone(),
                    format!("{}/announce.flac", self.http),
                ];
                self.announce_id = Some(self.player.play(Channel::Announce, urls));
            }
            Key::Pause => {
                println!("[{:6.2}] == pause MUSIC", self.ts());
                self.player.pause(Channel::Music);
                self.timers.set(Key::Resume, 1000);
            }
            Key::Resume => {
                println!(
                    "[{:6.2}] == resume MUSIC (state {})",
                    self.ts(),
                    sname(self.player.state(Channel::Music))
                );
                self.player.resume(Channel::Music);
            }
            Key::HttpsStop => {
                println!(
                    "[{:6.2}] https state after 5 s: {} -> stop()",
                    self.ts(),
                    sname(self.player.state(Channel::Music))
                );
                self.player.stop(Channel::Music);
                self.timers.set(Key::ExitAfterStop, 500);
            }
            Key::ExitAfterDone | Key::ExitAfterStop => return Step::Exit,
            Key::Timeout => {
                println!("[{:6.2}] TIMEOUT", self.ts());
                return Step::Exit;
            }
        }
        Step::Continue
    }

    fn event(&mut self, ev: PlayerEvent) -> Step {
        let (id, ok) = match ev {
            PlayerEvent::State(ch, s) => {
                self.saw_state = true;
                let name = if ch == Channel::Music {
                    "MUSIC"
                } else {
                    "ANNOUNCE"
                };
                println!("[{:6.2}] state {name} -> {}", self.ts(), sname(s));
                return Step::Continue;
            }
            PlayerEvent::Done { id, ok } => (Some(id), u8::from(ok)),
        };
        if id == self.music_id {
            // A working player reports LOADING before any list ends; the degraded one (dlopen or
            // gst_init failed, logged) fails every play() at once.
            if !self.saw_state {
                println!("player init failed");
                return Step::InitFailed;
            }
            println!("[{:6.2}] music done ok={ok}", self.ts());
            self.phase_http();
        } else if id == self.announce_id {
            println!("[{:6.2}] announce done ok={ok}, unduck", self.ts());
            self.player.set_channel_gain(Channel::Music, 0.3);
            self.timers.set(Key::Pause, 500);
        } else if id == self.tone_id {
            println!("[{:6.2}] http mp3 done ok={ok}", self.ts());
            println!("[{:6.2}] == HTTP 404 {}/missing.flac", self.ts(), self.http);
            let url = format!("{}/missing.flac", self.http);
            self.missing_id = Some(self.player.play(Channel::Music, vec![url]));
        } else if id == self.missing_id {
            println!("[{:6.2}] missing done ok={ok} (expect 0)", self.ts());
            self.phase_https();
        } else if id == self.https_id {
            println!("[{:6.2}] https done ok={ok}", self.ts());
            self.timers.set(Key::ExitAfterDone, 300);
        }
        Step::Continue
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "usage: {} <music_uri> <announce_file> <http_base> [https_url]",
            args[0]
        );
        return ExitCode::from(2);
    }
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let t0 = pulsar_core::now_ms();
    let ts = || (pulsar_core::now_ms() - t0) as f64 / 1000.0;

    let Some(model) = pulsar_core::model::detect_model() else {
        return ExitCode::FAILURE;
    };
    // Test sounds only with the volume capped: without the control there is no cap.
    let mut mx = Mixer::open(model);
    let ctl = pulsar_alsa::Ctl::open("hw:0").ok();
    let orig_raw = ctl.as_ref().and_then(read_raw);
    let (true, Some(ctl), Some(orig_raw)) = (mx.available(), ctl, orig_raw) else {
        println!("mixer unavailable: no volume cap, not playing");
        return ExitCode::FAILURE;
    };
    println!(
        "[{:6.2}] mixer volume {:.3} raw={orig_raw:?}",
        ts(),
        mx.volume()
    );
    if orig_raw.contains(&0) {
        println!("master muted (raw 0): not playing");
        return ExitCode::SUCCESS;
    }
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    // From here on these signals end the run (volume restored) instead of killing the process.
    let signals = {
        let _rt = rt.enter();
        [
            SignalKind::interrupt(),
            SignalKind::terminate(),
            SignalKind::hangup(),
        ]
        .map(signal)
    };
    let [Ok(mut sigint), Ok(mut sigterm), Ok(mut sighup)] = signals else {
        println!("cannot install signal handlers, not playing");
        return ExitCode::FAILURE;
    };
    *RESTORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((ctl, orig_raw));
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        default_hook(info);
    }));

    let volume = mx.volume().min(MAX_TEST_VOLUME);
    let cap = mixer::to_raw(model, MAX_TEST_VOLUME);
    let want = mixer::to_raw(model, volume);
    mx.set_volume(volume);
    println!(
        "[{:6.2}] set_volume({volume:.3}): expect values={want},{want}",
        ts()
    );
    amixer();
    mx.set_muted(true);
    println!("[{:6.2}] set_muted(true): expect values=0,0", ts());
    amixer();
    mx.set_muted(false);
    println!(
        "[{:6.2}] set_muted(false): expect values={want},{want}",
        ts()
    );
    amixer();
    let now_raw = RESTORE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|(ctl, _)| read_raw(ctl));
    match now_raw {
        Some(raw) if raw.iter().all(|&r| r <= cap) => {}
        raw => {
            println!("volume above the test cap ({raw:?} > {cap}), not playing");
            restore();
            return ExitCode::FAILURE;
        }
    }

    let (player, mut rx) = pulsar_media::spawn(model.playback_pcm);
    let mut probe = Probe {
        t0,
        music: args[1].clone(),
        announce: args[2].clone(),
        http: args[3].clone(),
        https: args.get(4).cloned().unwrap_or_default(),
        player,
        timers: Timers::new(),
        music_id: None,
        announce_id: None,
        tone_id: None,
        missing_id: None,
        https_id: None,
        saw_state: false,
    };
    let step = rt.block_on(async {
        probe.start();
        loop {
            let step = tokio::select! {
                biased;
                _ = sigint.recv() => Step::Signal,
                _ = sigterm.recv() => Step::Signal,
                _ = sighup.recv() => Step::Signal,
                key = probe.timers.next() => probe.timer(key),
                ev = rx.recv() => match ev {
                    Some(ev) => probe.event(ev),
                    None => Step::Exit,
                },
            };
            if !matches!(step, Step::Continue) {
                return step;
            }
        }
    });
    // Stops the GLib thread (pipelines to NULL) before the volume goes back.
    drop(probe);
    drop(rt);
    restore();
    amixer();
    match step {
        Step::InitFailed => ExitCode::FAILURE,
        Step::Signal => {
            println!("[{:6.2}] signal: exit", ts());
            ExitCode::FAILURE
        }
        _ => {
            println!("[{:6.2}] exit", ts());
            ExitCode::SUCCESS
        }
    }
}
