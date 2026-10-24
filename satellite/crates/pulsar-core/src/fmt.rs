//! Number formatting of `state.conf`, byte-compatible with the C++ daemon (`Settings::save` wrote
//! floats with a default ostream, `Settings::load` read them with strtof): C++ and Rust binaries
//! replace each other on the stations and roll back to `.prev`, both reading the same file.

use crate::sys;

/// Like printf "%g" (and a default C++ ostream): 6 significant digits, trailing zeros dropped,
/// exponent form below 1e-4 and from 1e6.
pub fn g6(v: f64) -> String {
    sys::format_g(v)
}

/// Like strtof: the numeric prefix after leading white space, 0 when there is none.
pub fn parse_f32_prefix(s: &str) -> f32 {
    sys::strtof(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g6_matches_printf() {
        assert_eq!(g6(0.5), "0.5");
        assert_eq!(g6(1.0), "1");
        assert_eq!(g6(1.0 / 3.0), "0.333333");
        assert_eq!(g6(0.15), "0.15");
        assert_eq!(g6(1e-7), "1e-07");
        assert_eq!(g6(123456789.0), "1.23457e+08");
        assert_eq!(g6(0.0), "0");
        assert_eq!(g6(-2.5), "-2.5");
        assert_eq!(g6(-1.0 / 3.0), "-0.333333");
        assert_eq!(g6(999999.0), "999999");
        assert_eq!(g6(999999.5), "1e+06");
        assert_eq!(g6(0.0001), "0.0001");
        assert_eq!(g6(0.00001), "1e-05");
        // Settings stores f32 values; widened to double as the ostream did.
        assert_eq!(g6(f64::from(0.3f32)), "0.3");
    }

    #[test]
    fn parse_f32_prefix_matches_strtof() {
        assert_eq!(parse_f32_prefix("0.3abc"), 0.3);
        assert_eq!(parse_f32_prefix(""), 0.0);
        assert_eq!(parse_f32_prefix("x"), 0.0);
        assert_eq!(parse_f32_prefix(" 1e2\n"), 100.0);
        assert_eq!(parse_f32_prefix("-0.25"), -0.25);
        assert_eq!(parse_f32_prefix("1\0junk"), 1.0);
    }
}
