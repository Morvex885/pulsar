//! Visualizer data (0..65535 = -60..0 dB, A-weighted) → `MusicFrame` levels.

/// The 8 bands of a `MusicFrame` from the server's spectrum bins, low to high: each band is the
/// mean of its share of the bins; with fewer than 8 bins, or a band without its own bin, it takes
/// the bin at its start. None without bins.
pub(crate) fn bands(bins: &[u16]) -> Option<[f32; 8]> {
    let n = bins.len();
    if n == 0 {
        return None;
    }
    let mut out = [0.0f32; 8];
    for (b, band) in out.iter_mut().enumerate() {
        let (lo, hi) = (b * n / 8, (b + 1) * n / 8);
        if n < 8 || hi <= lo {
            *band = f32::from(bins[lo.min(n - 1)]) / 65535.0;
            continue;
        }
        let sum = bins[lo..hi]
            .iter()
            .fold(0u32, |s, &v| s.wrapping_add(u32::from(v)));
        *band = sum as f32 / (hi - lo) as f32 / 65535.0;
    }
    Some(out)
}

pub(crate) fn loudness(v: u16) -> f32 {
    f32::from(v) / 65535.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;

    #[test]
    fn bands_match_cxx() {
        let mut n = 0;
        for f in golden::lines("spectrum") {
            let count: usize = f[0].parse().unwrap();
            let bins: Vec<u16> = f[1..=count].iter().map(|s| s.parse().unwrap()).collect();
            assert_eq!(f[count + 1], "=");
            let want = &f[count + 2..];
            match bands(&bins) {
                None => assert_eq!(want, ["none"]),
                Some(got) => {
                    let got: Vec<u32> = got.iter().map(|v| v.to_bits()).collect();
                    let want: Vec<u32> = want.iter().map(|s| golden::f32(s).to_bits()).collect();
                    assert_eq!(got, want, "{count} bins");
                }
            }
            n += 1;
        }
        assert_eq!(n, 8);
    }

    #[test]
    fn short_spectra_repeat_bins() {
        assert_eq!(bands(&[]), None);
        assert_eq!(bands(&[65535]), Some([1.0; 8]));
        // 3 bins: bands 0-2 take bin 0, 3-5 bin 1, 6-7 bin 2.
        let b = bands(&[0, 65535, 0]).unwrap();
        assert_eq!(b, [0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 0.0, 0.0]);
        // 9 bins: band 7 gets two of them.
        let mut bins = [0u16; 9];
        bins[7] = 65535;
        bins[8] = 0;
        assert_eq!(bands(&bins).unwrap()[7], 0.5);
        assert_eq!(loudness(65535), 1.0);
        assert_eq!(loudness(0), 0.0);
    }
}
