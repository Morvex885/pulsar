/// Ambient-light dimming factor shared by the clock and the LED ring: log-shaped (dark room
/// ~0.02, 10 lx ~0.4, >= 500 lx full), low-pass filtered, with hysteresis so that sensor noise
/// around a step boundary does not make the LEDs flicker between two levels.
#[derive(Clone, Debug)]
pub struct AmbientDimmer {
    smooth: f32,
    out: f32,
    primed: bool,
}

/// ~6 s time constant at one reading per 2 s.
const ALPHA: f32 = 0.3;
const HYSTERESIS: f32 = 0.03;

impl Default for AmbientDimmer {
    fn default() -> Self {
        Self::new()
    }
}

impl AmbientDimmer {
    pub fn new() -> Self {
        Self {
            smooth: 1.0,
            out: 1.0,
            primed: false,
        }
    }

    /// Feeds a lux reading (polled every ~2 s); true if `factor()` changed.
    pub fn update(&mut self, lux: f32) -> bool {
        let a = (1.0 + lux.max(0.0)).log10() / 501f32.log10();
        // The C++ station builds (clang -ffp-contract=on, aarch64 / cortex-a53) fused both
        // `a + b * c` below into one FMA; mul_add keeps their rounding.
        let target = a.min(1.0).mul_add(0.98, 0.02);
        self.smooth = if self.primed {
            (target - self.smooth).mul_add(ALPHA, self.smooth)
        } else {
            target
        };
        if self.primed && (self.smooth - self.out).abs() <= HYSTERESIS {
            return false;
        }
        self.primed = true;
        self.out = self.smooth;
        true
    }

    /// 1 until the first reading.
    pub fn factor(&self) -> f32 {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn first_reading_applies_at_once() {
        let mut d = AmbientDimmer::new();
        assert_eq!(d.factor(), 1.0);
        assert!(d.update(0.0));
        assert!(close(d.factor(), 0.02));

        let mut d = AmbientDimmer::new();
        assert!(d.update(1000.0));
        assert!(close(d.factor(), 1.0));
        // Negative readings count as darkness.
        let mut d = AmbientDimmer::new();
        assert!(d.update(-5.0));
        assert!(close(d.factor(), 0.02));
    }

    #[test]
    fn later_readings_are_smoothed() {
        let mut d = AmbientDimmer::new();
        d.update(500.0);
        assert!(d.update(0.0));
        assert!(close(d.factor(), 1.0 + 0.3 * (0.02 - 1.0)));
        assert!(d.update(0.0));
        assert!(close(d.factor(), 0.706 + 0.3 * (0.02 - 0.706)));
    }

    #[test]
    fn hysteresis_holds_small_changes() {
        let mut d = AmbientDimmer::new();
        d.update(0.0);
        // 0.5 lx targets ~0.0839: the first step moves the smoothed value only ~0.019.
        assert!(!d.update(0.5));
        assert!(close(d.factor(), 0.02));
        // The second step accumulates past 0.03 from the shown value.
        assert!(d.update(0.5));
        assert!(close(d.factor(), 0.0526));
        // Small steps back down stay within the band again.
        assert!(!d.update(0.4));
        assert!(close(d.factor(), 0.0526));
    }

    /// Readings where the fused and the unfused arithmetic round differently; expected bits
    /// from the C++ `AmbientDimmer` built with FMA contraction (unfused it gives 3cbae0ea,
    /// 3ccdc4b0, 3d6e24a3, 3d4d3b5a).
    #[test]
    fn rounds_like_the_fused_cxx_build() {
        let first = |lux: u32| {
            let mut d = AmbientDimmer::new();
            d.update(f32::from_bits(lux));
            d.factor().to_bits()
        };
        assert_eq!(first(0x3c93_74bd), 0x3cba_e0eb); // 0.018 lx
        assert_eq!(first(0x3d07_2b02), 0x3ccd_c4af); // 0.033 lx
        let second = |lux: u32| {
            let mut d = AmbientDimmer::new();
            d.update(0.0);
            d.update(f32::from_bits(lux));
            d.factor().to_bits()
        };
        assert_eq!(second(0x3f9e_b852), 0x3d6e_24a5); // 1.24 lx
        assert_eq!(second(0x3f63_d70a), 0x3d4d_3b59); // 0.89 lx
    }
}
