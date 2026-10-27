//! Volume curves shared by the controller and the Sendspin sink.

/// Master gain of a software-volume model (the Mini): the station has no usable ALSA master, so
/// the controller applies the volume to the player channels itself. The gain follows the stock
/// softvol of the other stations, linear in dB over 60 dB (level 1 = raw 49 of 255, -48.5 dB;
/// level 20 = raw 255, 0 dB), so one 5 % step sounds the same on every model. A linear amplitude
/// did not: 0.05 -> 0.10 was +6 dB, 0.95 -> 1.0 just +0.4 dB, and nearly the whole audible range
/// sat in the bottom divisions.
pub fn software_gain(volume: f32) -> f32 {
    let v = volume.clamp(0.0, 1.0);
    if v <= 0.0 {
        return 0.0;
    }
    // Platform level v * 20 on the stock 20-level map (0 -> 0, 1 -> 49, 20 -> 255), piecewise
    // linear as `Mixer::to_raw` does it on the models with a softvol.
    let level = f64::from(v.min(1.0) * 20.0);
    let raw = if level <= 1.0 {
        level * 49.0
    } else {
        49.0 + (level - 1.0) * (255.0 - 49.0) / (20.0 - 1.0)
    };
    let db = -60.0 + 60.0 * raw / 255.0;
    10f32.powf(db as f32 / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn software_gain_follows_the_stock_softvol_curve() {
        assert_eq!(software_gain(0.0), 0.0);
        assert_eq!(software_gain(1.0), 1.0);
        // Level 1 is raw 49, the bottom of the softvol of the other models: -48.47 dB.
        assert!((software_gain(0.05) - 0.0037711).abs() < 1e-7);
        // Every 5 % step adds the same 10.84 raw, i.e. 2.55 dB of the 60 dB range.
        let db = |a: f32, b: f32| 20.0 * (b / a).log10();
        let step_db = 60.0 * 206.0 / 19.0 / 255.0;
        for k in 1..20 {
            let a = software_gain(k as f32 / 20.0);
            let b = software_gain((k + 1) as f32 / 20.0);
            assert!((db(a, b) - step_db).abs() < 0.01, "step {k}");
        }
        // Clamped.
        assert_eq!(software_gain(2.0), 1.0);
        assert_eq!(software_gain(-1.0), 0.0);
    }
}
