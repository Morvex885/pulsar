//! Master output volume: the softvol "Master Playback Volume" of the model's playback PCM
//! (`Model::playback_pcm`) on card 0 (2 values, 0..255, linear in dB, Midi -60 dB..0 dB; TLV
//! mute=0, i.e. raw 0 is the bottom of the range, not silence), on the stock curve of the model
//! (`Model::volume_map`): volume v is platform level v * top level. The control is a user control
//! that alsa-lib creates the first time the playback PCM is opened, so [`Mixer::open`]
//! instantiates it if missing.
//!
//! The curves repeat the C++ arithmetic step by step (f32 / f64 / int as there): state.conf
//! values written by one binary are read by the other (`.prev`).

use log::{debug, error, info};
use pulsar_alsa::{Ctl, Pcm, Stream};
use pulsar_core::model::Model;

const MASTER: &str = "Master Playback Volume";

/// Raw control value of volume `v` (0..1). Level p = v * top level, piecewise linear through the
/// map. With `Model::volume_min_step` any audible volume is at least the lowest stock step (the
/// map's second point): the stock firmware never sets the raw values between it and 0.
pub fn to_raw(model: &Model, v: f32) -> i64 {
    let m = model.volume_map;
    if v <= 0.0 || m.len() < 2 {
        return 0;
    }
    // A float product in C++ (std::min(1.0f, v) * int), widened afterwards.
    let p = f64::from(v.min(1.0) * m[m.len() - 1].level as f32);
    if model.volume_min_step && p <= f64::from(m[1].level) {
        return i64::from(m[1].raw);
    }
    for w in m.windows(2) {
        let (a, b) = (w[0], w[1]);
        if p <= f64::from(b.level) {
            // No fused multiply-add here in the station builds: the product is divided first.
            let raw = f64::from(a.raw)
                + (p - f64::from(a.level)) * f64::from(b.raw - a.raw)
                    / f64::from(b.level - a.level);
            return raw.round() as i64;
        }
    }
    i64::from(m[m.len() - 1].raw)
}

/// Volume (0..1) of raw control value `raw`.
pub fn from_raw(model: &Model, raw: i64) -> f32 {
    let m = model.volume_map;
    if m.len() < 2 || raw <= i64::from(m[0].raw) {
        return 0.0;
    }
    for w in m.windows(2) {
        let (a, b) = (w[0], w[1]);
        if raw <= i64::from(b.raw) {
            // (raw - raw0) * (level1 - level0) is an int product in C++.
            let num = (raw - i64::from(a.raw)) * i64::from(b.level - a.level);
            let level = f64::from(a.level) + num as f64 / f64::from(b.raw - a.raw);
            return (level / f64::from(m[m.len() - 1].level)) as f32;
        }
    }
    1.0
}

/// Volume one platform level (`Model::volume_map`) above (`up`) or below `v`: the keys and HA's
/// volume up / down.
pub fn step_level(model: &Model, v: f32, up: bool) -> f32 {
    let m = model.volume_map;
    if m.len() < 2 {
        return clamp_unit(v);
    }
    grid_step(v, m[m.len() - 1].level, up)
}

/// Volume one Alice level above (`up`) or below `v`: "громче" / "тише". maind's Alice volume is
/// 0..10 (platform level / 2 rounded half up, on the 20-level maps).
pub fn step_alice(v: f32, up: bool) -> f32 {
    grid_step(v, 10, up)
}

/// The next point of a scale of `grid` steps above or below `v`. A volume between points (HA
/// slider, Music Assistant) moves to the next one, and repeated steps stay exact: f32 sums of
/// ±0.05 drift, and a volume a hair above 0 plays at the lowest stock step (`volume_min_step`)
/// while the clock and HA show 0.
fn grid_step(v: f32, grid: i32, up: bool) -> f32 {
    let p = clamp_unit(v) * grid as f32;
    // Within 1/1000 of a point is on it: earlier f32 steps leave ~1e-7 of error.
    let point = if up {
        (p + 1e-3).floor() as i32 + 1
    } else {
        (p - 1e-3).ceil() as i32 - 1
    };
    point.clamp(0, grid) as f32 / grid as f32
}

// The curve of builds before the stock map (alsamixer's "mapped volume" over a -60..0 dB
// control): dB = 60 * log10(0.1 + 0.9 v). state.conf keeps volume= on it for those builds.
fn legacy_raw(v: f32) -> i64 {
    if v <= 0.0 {
        return 0;
    }
    // The station builds (zig c++, -ffp-contract=on) fuse 0.1 + 0.9 * v into one fmadd / vfma.
    let db = 60.0 * 0.9f64.mul_add(f64::from(v.min(1.0)), 0.1).log10();
    let raw = ((db + 60.0) / 60.0 * 255.0).round() as i64;
    raw.clamp(0, 255)
}

fn legacy_volume(raw: i64) -> f32 {
    let db = -60.0 + 60.0 * raw as f64 / 255.0;
    ((10f64.powf(db / 60.0) - 0.1) / 0.9).max(0.0) as f32
}

/// Volume on the curve of builds before the stock map (state.conf `volume=`) -> the volume giving
/// the same raw level now (legacy levels below the lowest stock step rise to it).
pub fn from_legacy(model: &Model, v: f32) -> f32 {
    from_raw(model, legacy_raw(v))
}

/// Volume -> the volume on the legacy curve giving the same raw level.
pub fn to_legacy(model: &Model, v: f32) -> f32 {
    legacy_volume(to_raw(model, v))
}

/// 0..1 with NaN = 0, as C++ `std::min(1.0f, std::max(0.0f, v))`.
fn clamp_unit(v: f32) -> f32 {
    let v = if 0.0 < v { v } else { 0.0 };
    if v < 1.0 { v } else { 1.0 }
}

/// The master volume control. Without it (open failed, logged) `available()` is false and the
/// volume is kept in memory only.
pub struct Mixer {
    model: &'static Model,
    ctl: Option<Ctl>,
    volume: f32,
    muted: bool,
}

impl Mixer {
    /// Card 0; adopts the current raw value when it reads as > 0, else the volume stays 0.5.
    pub fn open(model: &'static Model) -> Mixer {
        let mut mixer = Mixer {
            model,
            ctl: None,
            volume: 0.5,
            muted: false,
        };
        let ctl = match Ctl::open("hw:0") {
            Ok(ctl) => ctl,
            Err(rc) => {
                error!("mixer: snd_ctl_open hw:0: {}", pulsar_alsa::strerror(rc));
                return mixer;
            }
        };
        if !ctl.mixer_exists(MASTER) {
            // Opening the softvol PCM once creates the control.
            drop(Pcm::open(model.playback_pcm, Stream::Playback, true));
            if !ctl.mixer_exists(MASTER) {
                error!("mixer: control '{MASTER}' not found");
                return mixer;
            }
        }
        if let Ok(raw) = ctl.read_integer(MASTER, 0) {
            if raw > 0 {
                mixer.volume = from_raw(model, raw);
            }
            info!("mixer: master raw={raw} -> volume {:.2}", mixer.volume);
        }
        mixer.ctl = Some(ctl);
        mixer
    }

    pub fn available(&self) -> bool {
        self.ctl.is_some()
    }

    /// 0..1
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// 0..1, written to the control at once.
    pub fn set_volume(&mut self, v: f32) {
        self.volume = clamp_unit(v);
        self.apply();
    }

    pub fn muted(&self) -> bool {
        self.muted
    }

    /// Mute = raw 0 (still -60 dB, not silence).
    pub fn set_muted(&mut self, m: bool) {
        self.muted = m;
        self.apply();
    }

    fn apply(&self) {
        let Some(ctl) = &self.ctl else { return };
        let raw = if self.muted {
            0
        } else {
            to_raw(self.model, self.volume)
        };
        match ctl.write_integers(MASTER, &[raw, raw]) {
            Err(rc) => error!("mixer: write {raw}: {}", pulsar_alsa::strerror(rc)),
            Ok(()) => debug!(
                "mixer: volume {:.2} muted={} -> raw {raw}",
                self.volume,
                i32::from(self.muted)
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsar_core::model::model_from_compatible;

    fn midi() -> &'static Model {
        model_from_compatible(b"yandex,cucumber\0").unwrap()
    }

    #[test]
    fn volume_is_clamped_like_cxx() {
        assert_eq!(clamp_unit(0.25), 0.25);
        assert_eq!(clamp_unit(-0.1), 0.0);
        assert_eq!(clamp_unit(1.5), 1.0);
        assert_eq!(clamp_unit(f32::NAN), 0.0);
        assert_eq!(clamp_unit(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn legacy_raw_ends() {
        assert_eq!(legacy_raw(0.0), 0);
        assert_eq!(legacy_raw(-1.0), 0);
        assert_eq!(legacy_raw(1.0), 255);
        assert_eq!(legacy_raw(f32::NAN), 255);
        assert_eq!(legacy_volume(255), 1.0);
        assert_eq!(legacy_volume(0), 0.0);
    }

    #[test]
    fn steps_from_a_level_move_exactly_one() {
        for c in [
            &b"yandex,cucumber\0"[..],
            b"yandex,plum\0",
            b"yandex,station-mini2\0",
        ] {
            let m = model_from_compatible(c).unwrap();
            for k in 0..=20 {
                let v = k as f32 / 20.0;
                assert_eq!(step_level(m, v, true), (k + 1).min(20) as f32 / 20.0);
                assert_eq!(step_level(m, v, false), (k - 1).max(0) as f32 / 20.0);
            }
        }
        for k in 0..=10 {
            let v = k as f32 / 10.0;
            assert_eq!(step_alice(v, true), (k + 1).min(10) as f32 / 10.0);
            assert_eq!(step_alice(v, false), (k - 1).max(0) as f32 / 10.0);
        }
    }

    #[test]
    fn steps_from_between_levels_go_to_the_next_level() {
        let m = midi();
        assert_eq!(step_level(m, 0.07, false), 0.05);
        assert_eq!(step_level(m, 0.07, true), 0.1);
        assert_eq!(step_level(m, 0.33, true), 0.35);
        assert_eq!(step_level(m, 0.33, false), 0.3);
        assert_eq!(step_alice(0.05, true), 0.1);
        assert_eq!(step_alice(0.15, false), 0.1);
        assert_eq!(step_alice(0.33, false), 0.3);
        // f32 drift right next to a level counts as that level.
        assert_eq!(step_level(m, 0.150_000_1, false), 0.1);
        assert_eq!(step_level(m, 0.149_999_9, true), 0.2);
        assert_eq!(step_alice(0.199_999_9, true), 0.3);
    }

    #[test]
    fn steps_down_reach_exact_silence() {
        let m = midi();
        let mut v = 0.15;
        for _ in 0..3 {
            v = step_level(m, v, false);
        }
        assert_eq!(v, 0.0);
        let mut v = 0.3;
        for _ in 0..3 {
            v = step_alice(v, false);
        }
        assert_eq!(v, 0.0);
        assert_eq!(step_level(m, 1.0, true), 1.0);
        assert_eq!(step_alice(0.0, false), 0.0);
    }
}
