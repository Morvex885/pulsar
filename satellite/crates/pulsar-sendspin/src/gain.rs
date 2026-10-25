//! Software duck gain of the Sendspin output: a linear ramp over 80 ms for a full-scale change,
//! applied per frame to 16-bit PCM (the only depth the player advertises). Float operations in
//! the order of the C++ sink, so both produce the same samples.

/// Full-scale ramp duration.
const RAMP_SEC: f32 = 0.08;

/// C++ `std::min`: `b < a ? b : a` (keeps `a` for NaN).
fn min(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

/// C++ `std::max`: `a < b ? b : a`.
fn max(a: f32, b: f32) -> f32 {
    if a < b { b } else { a }
}

/// Gain after `frames` frames of the ramp from `gain` toward `target` (rate 0 counts as 48 kHz).
pub(crate) fn ramp_end_gain(gain: f32, target: f32, rate: u32, frames: u32) -> f32 {
    let rate = if rate != 0 { rate } else { 48000 };
    let step = 1.0 / (RAMP_SEC * rate as f32);
    let d = step * frames as f32;
    if gain < target {
        min(target, gain + d)
    } else {
        max(target, gain - d)
    }
}

/// Scales `frames` interleaved S16_LE frames of `data` into `out`, ramping from `gain` toward
/// `target` one step per frame (`rate` must not be 0). Samples round to nearest even, as lrintf.
pub(crate) fn scale_s16(
    data: &[u8],
    frames: usize,
    channels: usize,
    gain: f32,
    target: f32,
    rate: u32,
    out: &mut Vec<u8>,
) {
    let frame_bytes = channels * 2;
    out.resize(frames * frame_bytes, 0);
    if frame_bytes == 0 {
        return;
    }
    let step = 1.0 / (RAMP_SEC * rate as f32);
    let mut g = gain;
    let frames_in = data.chunks_exact(frame_bytes);
    for (src, dst) in frames_in.zip(out.chunks_exact_mut(frame_bytes)) {
        if g < target {
            g = min(target, g + step);
        } else if g > target {
            g = max(target, g - step);
        }
        for (s, d) in src
            .as_chunks::<2>()
            .0
            .iter()
            .zip(dst.as_chunks_mut::<2>().0)
        {
            let v = f32::from(i16::from_le_bytes(*s)) * g;
            // |v| <= 32768 with g <= 1: the cast does not saturate.
            *d = (v.round_ties_even() as i16).to_le_bytes();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden;

    #[test]
    fn ramp_end_gain_matches_cxx() {
        let mut n = 0;
        for f in golden::lines("ramp") {
            let (g, t, rate, frames, want) = (
                golden::f32(f[0]),
                golden::f32(f[1]),
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
                golden::f32(f[4]),
            );
            let got = ramp_end_gain(g, t, rate, frames);
            assert_eq!(got.to_bits(), want.to_bits(), "{f:?}");
            n += 1;
        }
        assert_eq!(n, 10);
    }

    #[test]
    fn scale_s16_matches_cxx() {
        let lines: Vec<_> = golden::lines("scale")
            .into_iter()
            .chain(golden::lines("in"))
            .chain(golden::lines("out"))
            .collect();
        let cases = lines.len() / 3;
        assert_eq!(cases, 4);
        let mut out = Vec::new();
        for i in 0..cases {
            let (p, input, want) = (&lines[i], &lines[cases + i], &lines[2 * cases + i]);
            let (g, t, rate, ch, frames) = (
                golden::f32(p[0]),
                golden::f32(p[1]),
                p[2].parse().unwrap(),
                p[3].parse().unwrap(),
                p[4].parse().unwrap(),
            );
            let data: Vec<u8> = input
                .iter()
                .flat_map(|s| s.parse::<i16>().unwrap().to_le_bytes())
                .collect();
            scale_s16(&data, frames, ch, g, t, rate, &mut out);
            let got: Vec<i16> = out
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&b| i16::from_le_bytes(b))
                .collect();
            let want: Vec<i16> = want.iter().map(|s| s.parse().unwrap()).collect();
            assert_eq!(got, want, "{p:?}");
        }
    }

    #[test]
    fn full_scale_ramp_takes_80_ms() {
        // 48 kHz: 3840 frames from 1 to 0.
        assert!(ramp_end_gain(1.0, 0.0, 48000, 3840) < 1e-6);
        assert!(ramp_end_gain(1.0, 0.0, 48000, 3800) > 0.01);
        assert_eq!(ramp_end_gain(1.0, 0.0, 48000, 3841), 0.0);
        let data: Vec<u8> = std::iter::repeat_n(1000i16.to_le_bytes(), 3841)
            .flatten()
            .collect();
        let mut out = Vec::new();
        scale_s16(&data, 3841, 1, 1.0, 0.0, 48000, &mut out);
        assert_eq!(out.len(), 3841 * 2);
        assert_eq!(i16::from_le_bytes([out[0], out[1]]), 1000);
        assert_eq!(i16::from_le_bytes([out[7680], out[7681]]), 0);
    }
}
