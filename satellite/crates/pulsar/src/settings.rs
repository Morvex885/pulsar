//! `state.conf`: one `key=value` per line. Byte-compatible with the C++ daemon (controller.cpp
//! `Settings`), so a binary rolled back by supervise.sh, or rolled forward, reads what the other
//! wrote: numbers as C++ ostream writes them (`%g`, 6 significant digits) and reads them
//! (`strtof`), booleans `0` / `1`, `zigbee_flash_failed` in lowercase hex without `0x`.
//!
//! Writes are atomic and durable (tmp, fsync, rename, fsync of the directory): /data on the
//! Lite 2 is ubifs, where a rename reaches the flash long before unsynced data, so a power cut
//! could leave an empty file and reset every setting. `StateFile` writes a change
//! `SAVE_DELAY_MS` after the last one (a slider or a held volume key is one write, not dozens of
//! flash writes); keys that matter across a restart or a binary swap are written at once.

use std::ffi::OsString;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use log::warn;
use pulsar_core::Timers;
use pulsar_core::fmt::{g6, parse_f32_prefix};
use pulsar_esphome::LightState;
use pulsar_zigbee::Mode;

/// A change is written this long after the last one.
pub const SAVE_DELAY_MS: u64 = 1000;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Volume on the stock curve (`Model::volume_map`), `volume2=`. `volume=` keeps the same raw
    /// level on the curve of earlier builds, so a binary rolled back by supervise.sh plays as
    /// loud.
    pub volume: f32,
    /// `volume2=` was present.
    pub has_volume: bool,
    /// `volume=`; < 0: absent.
    pub volume_legacy: f32,
    /// `volume_curve=2`: one interim build wrote `volume=` on the stock curve.
    pub legacy_on_stock: bool,
    /// Media mute.
    pub muted: bool,
    /// Software microphone mute.
    pub mic_muted: bool,
    pub clock_on: bool,
    pub clock_brightness: f32,
    /// Clock level follows the light sensor, with `clock_brightness` as the ceiling.
    pub clock_auto_brightness: bool,
    /// Clock text scroll step.
    pub clock_scroll_ms: i32,
    /// User ring light.
    pub light: LightState,
    pub wake_word: String,
    /// POSIX TZ (the default matches what the Yandex firmware showed); updated from HA.
    pub tz: String,
    /// Install new releases without asking.
    pub auto_update: bool,
    /// Wake arbitration with other pulsar stations.
    pub nearest_answers: bool,
    /// One of `pulsar_hw::music_styles()`.
    pub music_viz: String,
    /// One of `pulsar_clock::music_styles()`.
    pub clock_music: String,
    /// 0.25..4
    pub led_speed: f32,
    /// Listening / thinking / speaking shown on the ring and the clock.
    pub voice_indication: bool,
    /// Audio between the end of the wake word and its confirmation goes ahead of the request.
    pub wake_preroll: bool,
    /// Stem of `pulsar_hw::case_colors()`, "" = none.
    pub led_case_color: String,
    /// Release being installed. Still set after a restart into another version: that release
    /// was rolled back by supervise.sh (crash loop), automatic updates skip it.
    pub update_attempt: String,
    pub zigbee_role: Mode,
    /// Radio image version given up on (the radio did not take it); not written again until the
    /// role changes or another image version is installed.
    pub zigbee_flash_failed: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            volume: 0.5,
            has_volume: false,
            volume_legacy: -1.0,
            legacy_on_stock: false,
            muted: false,
            mic_muted: false,
            clock_on: true,
            clock_brightness: 1.0,
            clock_auto_brightness: true,
            clock_scroll_ms: 400,
            light: LightState::default(),
            wake_word: "alisa".into(),
            tz: "MSK-3".into(),
            auto_update: false,
            nearest_answers: true,
            music_viz: "All styles".into(),
            clock_music: "Off".into(),
            led_speed: 1.0,
            voice_indication: true,
            wake_preroll: true,
            led_case_color: String::new(),
            update_attempt: String::new(),
            zigbee_role: Mode::Off,
            zigbee_flash_failed: 0,
        }
    }
}

impl Settings {
    /// Reads `path` over the current values; false when it cannot be read.
    pub fn load(&mut self, path: &Path) -> bool {
        match std::fs::read(path) {
            Ok(text) => {
                self.parse(&text);
                true
            }
            Err(_) => false,
        }
    }

    /// The C++ `Settings::load` loop: lines split at '\n' only (a '\r' stays in the value), the
    /// key up to the first '='; unknown keys and lines without '=' are skipped.
    ///
    /// Accepted difference: values are decoded as lossy UTF-8 (C++ kept raw bytes), so an
    /// invalid byte becomes U+FFFD. Such values never come from HA (its strings are UTF-8) nor
    /// from either daemon's own writes.
    pub fn parse(&mut self, text: &[u8]) {
        for line in text.split(|&b| b == b'\n') {
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let k = &line[..eq];
            let v = String::from_utf8_lossy(&line[eq + 1..]).into_owned();
            let fv = parse_f32_prefix(&v);
            let on = v == "1";
            match k {
                b"volume2" => {
                    self.volume = fv;
                    self.has_volume = true;
                }
                b"volume" => self.volume_legacy = fv,
                b"volume_curve" => self.legacy_on_stock = v == "2",
                b"muted" => self.muted = on,
                b"mic_muted" => self.mic_muted = on,
                b"clock_on" => self.clock_on = on,
                b"clock_brightness" => self.clock_brightness = fv,
                b"clock_auto_brightness" => self.clock_auto_brightness = on,
                // C++ int(float): truncation (Rust saturates where C++ is undefined).
                b"clock_scroll_ms" => self.clock_scroll_ms = (fv as i32).clamp(100, 2000),
                b"light_on" => self.light.on = on,
                b"light_brightness" => self.light.brightness = fv,
                b"light_r" => self.light.r = fv,
                b"light_g" => self.light.g = fv,
                b"light_b" => self.light.b = fv,
                b"light_effect" => self.light.effect = v,
                b"wake_word" => self.wake_word = v,
                b"tz" => self.tz = v,
                b"auto_update" => self.auto_update = on,
                b"nearest_answers" => self.nearest_answers = on,
                b"update_attempt" => self.update_attempt = v,
                b"zigbee_role" => self.zigbee_role = pulsar_zigbee::parse_mode(&v),
                b"zigbee_flash_failed" => self.zigbee_flash_failed = strtoul_hex(&v) as u32,
                b"music_viz" => self.music_viz = v,
                b"clock_music" => self.clock_music = v,
                b"led_speed" => {
                    self.led_speed = if fv.is_finite() {
                        fv.clamp(0.25, 4.0)
                    } else {
                        1.0
                    };
                }
                b"voice_indication" => self.voice_indication = on,
                b"wake_preroll" => self.wake_preroll = on,
                b"led_case_color" => self.led_case_color = v,
                _ => {}
            }
        }
        if !pulsar_hw::music_styles().contains(&self.music_viz.as_str()) {
            self.music_viz = "All styles".into();
        }
        if !pulsar_clock::music_styles().contains(&self.clock_music.as_str()) {
            self.clock_music = "Off".into();
        }
    }

    /// The file as C++ `Settings::save` writes it: every key, in its order.
    /// `wake_preroll` and `clock_auto_brightness` were added after the C++ reference fixtures.
    pub fn serialize(&self) -> String {
        let f = |v: f32| g6(f64::from(v));
        let b = |v: bool| u8::from(v);
        format!(
            "volume2={}\nvolume={}\nmuted={}\nmic_muted={}\nclock_on={}\nclock_brightness={}\n\
             clock_scroll_ms={}\nlight_on={}\nlight_brightness={}\nlight_r={}\nlight_g={}\n\
             light_b={}\nlight_effect={}\nwake_word={}\ntz={}\nauto_update={}\nupdate_attempt={}\n\
             zigbee_role={}\nzigbee_flash_failed={:x}\nnearest_answers={}\nmusic_viz={}\n\
             led_speed={}\nvoice_indication={}\nled_case_color={}\nclock_music={}\n\
             wake_preroll={}\nclock_auto_brightness={}\n",
            f(self.volume),
            f(self.volume_legacy),
            b(self.muted),
            b(self.mic_muted),
            b(self.clock_on),
            f(self.clock_brightness),
            self.clock_scroll_ms,
            b(self.light.on),
            f(self.light.brightness),
            f(self.light.r),
            f(self.light.g),
            f(self.light.b),
            self.light.effect,
            self.wake_word,
            self.tz,
            b(self.auto_update),
            self.update_attempt,
            pulsar_zigbee::role(self.zigbee_role).name,
            self.zigbee_flash_failed,
            b(self.nearest_answers),
            self.music_viz,
            f(self.led_speed),
            b(self.voice_indication),
            self.led_case_color,
            self.clock_music,
            b(self.wake_preroll),
            b(self.clock_auto_brightness),
        )
    }

    /// Atomic and durable: written to `<path>.tmp` (the C++ daemon's name; a leftover one is
    /// truncated) and fsynced, renamed over `path`, then the directory fsynced. A failed write
    /// is logged, its temporary file removed and `path` left as it was. False on failure.
    pub fn save(&self, path: &Path) -> bool {
        let mut tmp = OsString::from(path.as_os_str());
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = match File::create(&tmp) {
            Ok(f) => f,
            Err(e) => {
                warn!("cannot write {}: {e}", tmp.display());
                return false;
            }
        };
        let written = f
            .write_all(self.serialize().as_bytes())
            .and_then(|()| f.sync_all());
        // close(2) is not checked: fsync has already reported any write-back error.
        drop(f);
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
            warn!("cannot write {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        // The rename survives a power cut only once the directory is synced. Not a failure:
        // the new file is in place.
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
            warn!("cannot sync {}: {e}", dir.display());
        }
        true
    }
}

/// The `state.conf` writer: `changed` writes `SAVE_DELAY_MS` after the last change (when `due`
/// completes, the owner calls `flush`), `write_now` at once. Every write is the whole file, so
/// one write covers every change pending until then. A failed write stays pending: the next
/// change or `flush` tries again.
pub struct StateFile {
    path: PathBuf,
    dirty: bool,
    timer: Timers<()>,
}

impl StateFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            dirty: false,
            timer: Timers::new(),
        }
    }

    /// `st` changed: written `SAVE_DELAY_MS` after the last change.
    pub fn changed(&mut self) {
        self.dirty = true;
        self.timer.set((), SAVE_DELAY_MS);
    }

    /// Writes `st` now; a pending delayed write is no longer needed. True = on disk; false = the
    /// change stays pending (the next change or `flush` tries again).
    #[must_use = "a write that must be on disk has to check it"]
    pub fn write_now(&mut self, st: &Settings) -> bool {
        self.timer.cancel(());
        self.dirty = !st.save(&self.path);
        !self.dirty
    }

    /// Writes `st` if a change is pending.
    pub fn flush(&mut self, st: &Settings) {
        if self.dirty {
            // A failure is logged by save and stays pending for the next change or flush.
            let _ = self.write_now(st);
        }
    }

    /// Completes when the delayed write is due; pending while none is. Cancel-safe.
    pub async fn due(&mut self) {
        self.timer.next().await;
    }
}

/// `strtoul(s, nullptr, 16)`: leading white space, an optional sign, an optional `0x`, then hex
/// digits; out of range gives `ULONG_MAX` (whose width is the target's), a minus sign negates.
fn strtoul_hex(s: &str) -> std::ffi::c_ulong {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    if b.get(i) == Some(&b'0')
        && matches!(b.get(i + 1), Some(b'x' | b'X'))
        && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        i += 2;
    }
    let mut v: std::ffi::c_ulong = 0;
    let mut overflow = false;
    while let Some(d) = b.get(i).and_then(|&c| (c as char).to_digit(16)) {
        match v
            .checked_mul(16)
            .and_then(|x| x.checked_add(std::ffi::c_ulong::from(d)))
        {
            Some(x) => v = x,
            None => overflow = true,
        }
        i += 1;
    }
    if overflow {
        std::ffi::c_ulong::MAX
    } else if neg {
        v.wrapping_neg()
    } else {
        v
    }
}

#[cfg(test)]
mod tests;
