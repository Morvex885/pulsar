//! Volume curves shared by the controller, the mixer and the Sendspin sink.

use crate::model::Model;

/// Raw control value of volume `v` (0..1). Level p = v * top level, piecewise linear through the
/// model's map. With `Model::volume_min_step` any audible volume is at least the lowest stock step
/// (the map's second point): the stock firmware never sets the raw values between it and 0.
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

/// Master gain of a software-volume model (the Mini): the controller applies the volume to the
/// player channels itself because the hardware "Master" does not attenuate the output. The gain is
/// derived from the same curve as the softvol of the other models (60 dB from raw 0 to 255), so a
/// volume step sounds the same on every station.
pub fn master_gain(model: &Model, v: f32) -> f32 {
    if !model.software_volume {
        return 1.0;
    }
    let raw = to_raw(model, v);
    if raw <= 0 {
        return 0.0;
    }
    let db = -60.0 + 60.0 * raw as f64 / 255.0;
    10f64.powf(db / 20.0) as f32
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
/// the same raw level now (legacy levels below the lowest stock step rise to it). Software-volume
/// models store the volume itself, not a raw control value.
pub fn from_legacy(model: &Model, v: f32) -> f32 {
    if model.software_volume {
        return clamp_unit(v);
    }
    from_raw(model, legacy_raw(v))
}

/// Volume -> the volume on the legacy curve giving the same raw level.
pub fn to_legacy(model: &Model, v: f32) -> f32 {
    if model.software_volume {
        return clamp_unit(v);
    }
    legacy_volume(to_raw(model, v))
}

/// 0..1 with NaN = 0, as C++ `std::min(1.0f, std::max(0.0f, v))`.
pub fn clamp_unit(v: f32) -> f32 {
    let v = if 0.0 < v { v } else { 0.0 };
    if v < 1.0 { v } else { 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::model_from_compatible;

    #[test]
    fn master_gain_follows_the_stock_softvol_curve() {
        let mini = model_from_compatible(b"yandex,station-mini2\0").unwrap();
        // master_gain only applies to software-volume models.
        assert_eq!(master_gain(mini, 0.2), 1.0);
        let mini = crate::model::model_from_dt_id(b"axg_s420_v03\0").unwrap();
        assert_eq!(master_gain(mini, 0.0), 0.0);
        assert_eq!(master_gain(mini, 1.0), 1.0);
        // Level 1 is raw 49, the bottom of the softvol of the other models: -48.47 dB.
        assert!((master_gain(mini, 0.05) - 0.0037711).abs() < 1e-7);
        // Every 5 % step adds ~10.84 raw, i.e. ~2.55 dB of the 60 dB range; the raw value is an
        // integer, so individual steps differ by up to half a raw step (~0.12 dB).
        let db = |a: f32, b: f32| 20.0 * (b / a).log10();
        let step_db = 60.0 * 206.0 / 19.0 / 255.0;
        for k in 1..20 {
            let a = master_gain(mini, k as f32 / 20.0);
            let b = master_gain(mini, (k + 1) as f32 / 20.0);
            assert!((db(a, b) - step_db).abs() < 0.2, "step {k}");
        }
        assert_eq!(master_gain(mini, 2.0), 1.0);
        assert_eq!(master_gain(mini, -1.0), 0.0);
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
    fn legacy_curve_ends() {
        assert_eq!(legacy_raw(0.0), 0);
        assert_eq!(legacy_raw(-1.0), 0);
        assert_eq!(legacy_raw(1.0), 255);
        assert_eq!(legacy_raw(f32::NAN), 255);
        assert_eq!(legacy_volume(255), 1.0);
        assert_eq!(legacy_volume(0), 0.0);
    }
}
