//! Decisions of the controller that depend only on their inputs (controller.cpp), kept apart for
//! the host tests.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

use pulsar_core::model::Model;
use pulsar_esphome::{DeviceIdentity, MediaState, VaEvent};
use pulsar_hw::Rgb;
use pulsar_media::PlayState;

/// After the wake word a stock command ("Алиса, громче") is handled locally if it is heard
/// within this window; the HA pipeline is then cancelled.
pub const COMMAND_AFTER_WAKE_MS: i64 = 2500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Listening,
    Processing,
    Speaking,
}

/// C++ `std::min`: `b < a ? b : a` (a NaN `b` gives `a`).
pub fn cxx_min(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

/// C++ `std::max`: `a < b ? b : a` (a NaN `b` gives `a`).
pub fn cxx_max(a: f32, b: f32) -> f32 {
    if a < b { b } else { a }
}

/// C++ `std::clamp`: a NaN passes through.
pub fn cxx_clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}

/// Base ports and `PULSAR_PORT_OFFSET` (a test instance next to the running daemon).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub offset: u16,
    pub api: u16,
    pub sendspin: u16,
    pub deconz: u16,
    pub arbiter: u16,
}

pub const API_PORT: u16 = 6053;
pub const SENDSPIN_PORT: u16 = 8928;
/// deCONZ-protocol Zigbee coordinator (ZHA socket:// / Zigbee2MQTT tcp://); 6638 is the port
/// network Zigbee coordinators conventionally use.
pub const DECONZ_PORT: u16 = 6638;
pub const ARBITER_PORT: u16 = 6054;

/// `offset`: the variable's value, None when unset. None: not a u16, or a port overflows.
pub fn ports(offset: Option<&OsStr>) -> Option<Ports> {
    let offset = match offset {
        None => 0,
        Some(s) => s.to_str()?.parse::<u16>().ok()?,
    };
    Some(Ports {
        offset,
        api: API_PORT.checked_add(offset)?,
        sendspin: SENDSPIN_PORT.checked_add(offset)?,
        deconz: DECONZ_PORT.checked_add(offset)?,
        arbiter: ARBITER_PORT.checked_add(offset)?,
    })
}

/// C++ `state_path_.substr(0, rfind('/'))`, "." without a slash.
pub fn state_dir(state_path: &OsStr) -> std::path::PathBuf {
    let b = state_path.as_bytes();
    match b.iter().rposition(|&c| c == b'/') {
        Some(i) => OsStr::from_bytes(&b[..i]).into(),
        None => ".".into(),
    }
}

/// `<dir>/<name>` as the C++ string concatenation (`dir + "/" + name`).
pub fn in_dir(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let mut s = dir.as_os_str().to_owned();
    s.push("/");
    s.push(name);
    s.into()
}

/// The ESPHome device: `host_prefix` + the last 6 hex digits of the wlan0 MAC (lowercase), the
/// MAC itself in uppercase. `mac_line`: the first line of /sys/class/net/wlan0/address.
pub fn identity(
    model: &Model,
    mac_line: &str,
    version: &str,
    compilation_time: &str,
) -> DeviceIdentity {
    let mac = mac_line.to_ascii_uppercase();
    let plain: String = mac
        .chars()
        .filter(|&c| c != ':')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let suffix = if plain.len() >= 6 {
        &plain[plain.len() - 6..]
    } else {
        &plain[..]
    };
    DeviceIdentity {
        name: format!("{}{suffix}", model.host_prefix),
        friendly_name: model.friendly_name.into(),
        mac,
        model: model.model_name.into(),
        board: model.codename.into(),
        manufacturer: "Yandex".into(),
        project_version: version.into(),
        compilation_time: compilation_time.into(),
    }
}

/// The volume to start with and whether it was migrated from `volume=` of an earlier build
/// (then persisted). `volume2=` is authoritative; `volume=` keeps the raw level of the
/// pre-curve mapping (`from_legacy`), or of the stock curve for `volume_curve=2`. With neither,
/// the speaker's own volume, but never loud: on the Lite 2 the softvol control is at 0 dB
/// (100 %) after boot until the Yandex stack sets it; 0.3 is stock level 6 of 20.
pub fn initial_volume(
    has_volume: bool,
    volume: f32,
    volume_legacy: f32,
    legacy_on_stock: bool,
    mixer_volume: f32,
    from_legacy: impl Fn(f32) -> f32,
) -> (f32, bool) {
    if has_volume {
        (volume, false)
    } else if volume_legacy >= 0.0 {
        let v = if legacy_on_stock {
            cxx_min(1.0, volume_legacy)
        } else {
            from_legacy(volume_legacy)
        };
        (v, true)
    } else {
        (cxx_min(mixer_volume, 0.3), false)
    }
}

/// `led_show` colour: "" = white, else `#RRGGBB` or `RRGGBB`. None: malformed.
pub fn parse_led_color(color: &str) -> Option<Rgb> {
    if color.is_empty() {
        return Some(Rgb {
            r: 255,
            g: 255,
            b: 255,
        });
    }
    let hex = color.strip_prefix('#').unwrap_or(color);
    if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(Rgb {
        r: (v >> 16) as u8,
        g: (v >> 8) as u8,
        b: v as u8,
    })
}

/// An HA voice timer as the controller keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HaTimer {
    pub active: bool,
    /// While active.
    pub end_ms: i64,
    /// While paused.
    pub seconds_left: u32,
}

/// The timer the clock shows: the running one that ends first, else the paused one with the
/// least time left; ties go to the smallest id. Timers that ran out locally (HA's FINISHED
/// follows) are skipped. Returns its id, data and the ms left.
pub fn pick_timer(timers: &BTreeMap<String, HaTimer>, now: i64) -> Option<(&str, HaTimer, i64)> {
    let mut best: Option<(&str, HaTimer, i64)> = None;
    for (id, tm) in timers {
        let left = if tm.active {
            tm.end_ms - now
        } else {
            i64::from(tm.seconds_left) * 1000
        };
        if left <= 0 {
            continue;
        }
        let better = match &best {
            None => true,
            Some((_, b, bl)) => (tm.active && !b.active) || (tm.active == b.active && left < *bl),
        };
        if better {
            best = Some((id, *tm, left));
        }
    }
    best
}

/// Stock command spotter words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceCommand {
    Stop,
    Pause,
    Resume,
    Louder,
    Quieter,
    Next,
    Prev,
}

/// None: TV navigation, "включи свет" etc., left to Home Assistant.
pub fn command_of(word: &str) -> Option<VoiceCommand> {
    Some(match word {
        "стоп" | "хватит" => VoiceCommand::Stop,
        "пауза" => VoiceCommand::Pause,
        "продолжи" | "играй" => VoiceCommand::Resume,
        "громче" => VoiceCommand::Louder,
        "тише" => VoiceCommand::Quieter,
        "дальше" | "следующий" => VoiceCommand::Next,
        "назад" => VoiceCommand::Prev,
        _ => return None,
    })
}

/// Within the window after the wake word that opened this LISTENING phase.
pub fn after_wake(phase: Phase, now: i64, listen_started_ms: i64) -> bool {
    phase == Phase::Listening && now - listen_started_ms < COMMAND_AFTER_WAKE_MS
}

/// The command spotter runs without the wake word while something plays (music, Sendspin,
/// announcement, timer ring, answer), right after the wake word and while arbitrating; never
/// while a microphone is muted.
pub fn commands_enabled(
    mic_muted: bool,
    playing: bool,
    after_wake: bool,
    arbitrating: bool,
) -> bool {
    !mic_muted && (playing || after_wake || arbitrating)
}

/// HA media player state. ANNOUNCING only for HA announcements, not for earcons, timer alarms or
/// pipeline TTS on the same channel.
pub fn media_state(announcing: bool, music: PlayState, sendspin_streaming: bool) -> MediaState {
    if announcing {
        return MediaState::Announcing;
    }
    match music {
        PlayState::Playing | PlayState::Loading => MediaState::Playing,
        PlayState::Paused if sendspin_streaming => MediaState::Playing,
        PlayState::Paused => MediaState::Paused,
        _ if sendspin_streaming => MediaState::Playing,
        _ => MediaState::Idle,
    }
}

/// Music (HA media channel and Sendspin) is ducked while a dialog, HA announcement, timer ring
/// or closing earcon is on.
pub fn want_duck(
    phase: Phase,
    announcing: bool,
    timer_ringing: bool,
    closing_earcon: bool,
) -> bool {
    phase != Phase::Idle || announcing || timer_ringing || closing_earcon
}

/// Restarting cuts off whatever plays: automatic installs wait for silence.
pub fn idle_for_update(
    phase: Phase,
    announcing: bool,
    timer_ringing: bool,
    music: PlayState,
    sendspin_streaming: bool,
    zigbee_flashing: bool,
) -> bool {
    phase == Phase::Idle
        && !announcing
        && !timer_ringing
        && music != PlayState::Playing
        && !sendspin_streaming
        && !zigbee_flashing
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoUpdate {
    No,
    /// Try again later (10 min): the speaker is busy.
    WaitIdle,
    Install,
}

/// `release`: latest release found, "" = none. Needs a parseable current version; a tag that
/// was rolled back (`update_attempt` still set to it) is skipped.
pub fn auto_update(
    enabled: bool,
    release: &str,
    installing: bool,
    current: &str,
    update_attempt: &str,
    idle: bool,
) -> AutoUpdate {
    if !enabled || release.is_empty() || installing {
        return AutoUpdate::No;
    }
    let (newer, current_known) = pulsar_update::release_newer(release, current);
    if !newer || !current_known || release == update_attempt {
        AutoUpdate::No
    } else if !idle {
        AutoUpdate::WaitIdle
    } else {
        AutoUpdate::Install
    }
}

/// A binary replaced under the running daemon (installer, manual mv) reads "<path> (deleted)":
/// the install location is still <path>.
pub fn strip_deleted(exe: &OsStr) -> &OsStr {
    const DELETED: &[u8] = b" (deleted)";
    let b = exe.as_bytes();
    if b.len() > DELETED.len() && b.ends_with(DELETED) {
        OsStr::from_bytes(&b[..b.len() - DELETED.len()])
    } else {
        exe
    }
}

/// Pipeline events carry no run id. HA ends a run it replaces (barge-in: our new start cancels
/// the old run) with a RUN_END sent before the new run's RUN_START, so until RUN_START every
/// event belongs to the replaced run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunBinding {
    /// This dialog's start request has no RUN_START yet.
    pub awaiting_run_start: bool,
    /// Some run is between RUN_START and RUN_END.
    pub run_active: bool,
    /// The RUN_END of a replaced run is still due.
    pub stale_run_end: bool,
}

impl RunBinding {
    /// A dialog sent its start request.
    pub fn start(&mut self) {
        self.stale_run_end = self.run_active;
        self.awaiting_run_start = true;
    }

    /// No more events come for the current run (client gone, voice unsubscribed).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Accounts for `ev`; false when it belongs to the run this dialog replaced.
    pub fn accept(&mut self, ev: VaEvent) -> bool {
        if ev == VaEvent::RunStart {
            self.run_active = true;
            self.awaiting_run_start = false;
        } else if self.awaiting_run_start {
            if ev != VaEvent::RunEnd || self.stale_run_end {
                if ev == VaEvent::RunEnd {
                    self.stale_run_end = false;
                    self.run_active = false;
                }
                return false;
            }
            // A RUN_END with no run to replace: HA ended our request without running it (it
            // intercepts the wake word while an automation waits for one).
            self.awaiting_run_start = false;
        } else if ev == VaEvent::RunEnd {
            self.run_active = false;
        }
        true
    }
}

/// Illuminance publishing: the first reading, a change by more than max(1 lx, 10 %), or 60 s
/// after the last one published. `last` < 0: none published yet.
pub fn publish_lux(lux: f32, last: f32, last_ms: i64, now: i64) -> bool {
    let diff = (lux - last).abs();
    last < 0.0 || diff > cxx_max(1.0, last * 0.1) || now - last_ms > 60_000
}

#[cfg(test)]
mod tests;
