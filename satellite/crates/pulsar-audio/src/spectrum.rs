//! Music features from the speaker reference channels of the capture (what the station itself
//! plays, after the softvol): 8 bands, loudness, beats. One VQE chunk per call (512 samples at
//! 16 kHz after decimation, 256 on the Mini 3 and Mini); the FFT runs every 512 samples, on the
//! front-end's processing thread.
//!
//! Rounding follows the C++ daemon as built for the stations: zig c++ (clang,
//! -ffp-contract=on) fuses `a ± b * c` within one expression into a fused multiply-add on aarch64
//! and on armhf (cortex-a53, VFPv4). The aarch64 / armhf assembly of spectrum.cpp has exactly
//! four: the Hann window, std::norm, both updates of the bass mean; those are `mul_add` here.
//! The complex product is not fused (armhf's vmla / vmls round in between). Without that a
//! 1-ulp difference could flip a beat decision.
//!
//! sinf / cosf / log10f are the platform's: every zig-linked C++ executable carried zig
//! compiler_rt's (weak definitions pulled in with its other helpers); the Rust armhf link pulls
//! them in the same way, the aarch64 one resolves glibc's. Those two differ by an ulp at times
//! (5 of the 512 window values; on the golden test data band values in 47 of 456 chunks, never a
//! beat).

use pulsar_core::MusicFrame;

const N: usize = 512;
const BANDS: usize = 8;

// The C++ constant 3.14159265358979f is this f32 (the nearest one to pi).
const PI: f32 = std::f32::consts::PI;
/// Band value 0 this far under the running peak.
const RANGE_DB: f32 = 45.0;
/// Per 32 ms chunk: the peak follows a fade-out at 3 dB/s.
const DECAY_DB: f32 = 0.096;
const FLOOR_DB: f32 = -70.0;
/// Total power under this: nothing is playing.
const SILENCE_DB: f32 = -75.0;
/// ~1 s mean of the bass power.
const BASS_ALPHA: f32 = 0.031;
/// Chunks (~250 ms) between beats.
const BEAT_GAP: i32 = 8;

fn to_db(p: f32) -> f32 {
    10.0 * (p + 1e-12).log10()
}

/// std::complex<float> arithmetic as libc++ computes it for finite values.
#[derive(Clone, Copy, Debug, Default)]
struct C32 {
    re: f32,
    im: f32,
}

impl C32 {
    fn add(self, o: C32) -> C32 {
        C32 {
            re: self.re + o.re,
            im: self.im + o.im,
        }
    }

    fn sub(self, o: C32) -> C32 {
        C32 {
            re: self.re - o.re,
            im: self.im - o.im,
        }
    }

    /// libc++ multiplies std::complex<float> as `_Complex float`: (ac - bd, ad + bc), not fused.
    fn mul(self, o: C32) -> C32 {
        let (ac, bd) = (self.re * o.re, self.im * o.im);
        let (ad, bc) = (self.re * o.im, self.im * o.re);
        C32 {
            re: ac - bd,
            im: ad + bc,
        }
    }

    /// std::norm: |z|², `re * re + im * im` with the first product fused.
    fn norm(self) -> f32 {
        self.re.mul_add(self.re, self.im * self.im)
    }
}

pub struct SpectrumAnalyzer {
    decim: usize,
    chunk_samples: usize,
    buffered: usize,
    window: [f32; N],
    twiddle: [C32; N / 2],
    /// Band b = bins lo[b] ..= hi[b].
    lo: [usize; BANDS],
    hi: [usize; BANDS],
    buf: [C32; N],
    /// Running peak, dB; set by reset().
    ref_db: f32,
    bass_mean: f32,
    since_beat: i32,
}

impl SpectrumAnalyzer {
    /// `chunk_frames` capture frames at `rate` per call.
    pub fn new(rate: u32, chunk_frames: usize) -> Self {
        let mut a = SpectrumAnalyzer {
            decim: 0,
            chunk_samples: 0,
            buffered: 0,
            window: [0.0; N],
            twiddle: [C32::default(); N / 2],
            lo: [0; BANDS],
            hi: [0; BANDS],
            buf: [C32::default(); N],
            ref_db: 0.0,
            bass_mean: 0.0,
            since_beat: 0,
        };
        a.reset();
        if rate == 0 || !rate.is_multiple_of(16000) {
            return a;
        }
        let decim = (rate / 16000) as usize;
        let chunk_samples = chunk_frames / decim;
        if chunk_samples == 0
            || !N.is_multiple_of(chunk_samples)
            || !chunk_frames.is_multiple_of(decim)
        {
            return a;
        }
        a.decim = decim;
        a.chunk_samples = chunk_samples;
        for (i, w) in a.window.iter_mut().enumerate() {
            *w = (-0.5f32).mul_add((2.0 * PI * i as f32 / N as f32).cos(), 0.5);
        }
        for (k, t) in a.twiddle.iter_mut().enumerate() {
            let theta = -2.0 * PI * k as f32 / N as f32;
            *t = C32 {
                re: theta.cos(),
                im: theta.sin(),
            };
        }
        let bin_hz = 16000.0 / N as f32;
        for b in 0..BANDS {
            let lo = 40.0 * (7600.0f32 / 40.0).powf(b as f32 / 8.0);
            let hi = 40.0 * (7600.0f32 / 40.0).powf((b + 1) as f32 / 8.0);
            a.lo[b] = (lo / bin_hz).ceil() as usize;
            a.hi[b] = a.lo[b].max((hi / bin_hz).floor() as usize);
        }
        a
    }

    /// False unless the chunk at 16 kHz divides the 512-sample FFT window (rate a multiple of
    /// 16 kHz).
    pub fn ok(&self) -> bool {
        self.decim != 0
    }

    pub fn reset(&mut self) {
        self.buffered = 0;
        self.ref_db = FLOOR_DB;
        self.bass_mean = 0.0;
        self.since_beat = 1000;
    }

    /// `spk`: the chunk's frames of `nspk` interleaved channels in [-1, 1]. True when `out` was
    /// filled; chunks shorter than 512 samples fill one FFT window over several calls.
    pub fn process(&mut self, spk: &[f32], nspk: usize, out: &mut MusicFrame) -> bool {
        if self.decim == 0 || nspk == 0 {
            return false;
        }
        let step = self.decim * nspk;
        if spk.len() < self.chunk_samples * step {
            return false;
        }
        let end = self.buffered + self.chunk_samples;
        let norm = 1.0 / step as f32;
        for ((b, frame), w) in self.buf[self.buffered..end]
            .iter_mut()
            .zip(spk.chunks_exact(step))
            .zip(&self.window[self.buffered..end])
        {
            let mut s = 0.0f32;
            for &x in frame {
                s += x;
            }
            *b = C32 {
                re: s * norm * w,
                im: 0.0,
            };
        }

        self.buffered = end;
        if self.buffered < N {
            return false;
        }
        self.buffered = 0;

        // In-place iterative radix-2 FFT.
        let mut j = 0usize;
        for i in 1..N {
            let mut bit = N >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j ^= bit;
            if i < j {
                self.buf.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= N {
            let step = N / len;
            for i in (0..N).step_by(len) {
                for k in 0..len / 2 {
                    let u = self.buf[i + k];
                    let v = self.buf[i + k + len / 2].mul(self.twiddle[k * step]);
                    self.buf[i + k] = u.add(v);
                    self.buf[i + k + len / 2] = u.sub(v);
                }
            }
            len <<= 1;
        }

        // A full-scale sine through the Hann window peaks at N/4: that is 0 dB.
        let scale = 16.0 / (N * N) as f32;
        let mut power = [0.0f32; BANDS];
        let mut total = 0.0f32;
        let mut max_db = FLOOR_DB;
        for (b, pw) in power.iter_mut().enumerate() {
            let (lo, hi) = (self.lo[b], self.hi[b]);
            let mut p = 0.0f32;
            for c in &self.buf[lo..=hi] {
                p += c.norm();
            }
            *pw = p * scale / (hi - lo + 1) as f32;
            total += *pw;
            max_db = max_db.max(to_db(*pw));
        }
        *out = MusicFrame::default();
        self.since_beat += 1;
        if to_db(total) < SILENCE_DB {
            self.bass_mean = BASS_ALPHA.mul_add(0.0 - self.bass_mean, self.bass_mean);
            self.ref_db = FLOOR_DB.max(self.ref_db - DECAY_DB);
            return true;
        }
        self.ref_db = max_db.max(self.ref_db - DECAY_DB).max(FLOOR_DB);
        let ref_db = self.ref_db;
        let value = |p: f32| ((to_db(p) - (ref_db - RANGE_DB)) / RANGE_DB).clamp(0.0, 1.0);
        for (band, &p) in out.bands.iter_mut().zip(&power) {
            *band = value(p);
        }
        out.loudness = value(total);

        let bass = power[0] + power[1];
        if bass > 1.5 * self.bass_mean && value(bass) > 0.3 && self.since_beat >= BEAT_GAP {
            out.beat = true;
            self.since_beat = 0;
        }
        self.bass_mean = BASS_ALPHA.mul_add(bass - self.bass_mean, self.bass_mean);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_is_the_cxx_constant() {
        #[allow(clippy::excessive_precision, clippy::approx_constant)]
        let cxx: f32 = 3.14159265358979;
        assert_eq!(PI.to_bits(), cxx.to_bits());
    }

    #[test]
    fn supported_layouts() {
        // Midi: 96 kHz, chunk 3072; Lite 2: 48 kHz, chunk 1536; Mini 3 and Mini: 48 kHz, chunk 768.
        assert!(SpectrumAnalyzer::new(96000, 3072).ok());
        assert!(SpectrumAnalyzer::new(48000, 1536).ok());
        assert!(SpectrumAnalyzer::new(48000, 768).ok());
        assert!(SpectrumAnalyzer::new(16000, 512).ok());
        assert!(!SpectrumAnalyzer::new(44100, 1536).ok());
        assert!(!SpectrumAnalyzer::new(0, 512).ok());
        assert!(!SpectrumAnalyzer::new(48000, 1024).ok());
        assert!(!SpectrumAnalyzer::new(48000, 1537).ok());
        assert!(!SpectrumAnalyzer::new(48000, 1152).ok());
        let mut a = SpectrumAnalyzer::new(44100, 1536);
        let mut f = MusicFrame::default();
        assert!(!a.process(&[0.0; 1536], 1, &mut f));
    }

    fn assert_frame_bits_eq(got: &MusicFrame, want: &MusicFrame) {
        assert_eq!(got.beat, want.beat);
        assert_eq!(got.loudness.to_bits(), want.loudness.to_bits());
        assert_eq!(got.bands.map(f32::to_bits), want.bands.map(f32::to_bits));
    }

    #[test]
    fn mini3_reference_matches_lite2_at_the_same_32ms_cadence() {
        // Microphones are not analyzer inputs: Mini 3's 3+1 and Lite 2's 2+1 share this reference.
        let mut mini3 = SpectrumAnalyzer::new(48000, 768);
        let mut lite2 = SpectrumAnalyzer::new(48000, 1536);
        let mut got = MusicFrame::default();
        let mut want = MusicFrame::default();
        let mut audible = false;
        let mut beats = 0;
        for chunk in 0..64 {
            let spk: Vec<f32> = (0..1536)
                .map(|i| {
                    let n = (chunk * 1536 + i) as f32;
                    let amp = if chunk % 16 < 2 { 0.5 } else { 0.01 };
                    amp * (2.0 * PI * 80.0 * n / 48000.0).sin()
                })
                .collect();
            let previous = got;
            assert!(!mini3.process(&spk[..768], 1, &mut got));
            assert_frame_bits_eq(&got, &previous);
            assert!(mini3.process(&spk[768..], 1, &mut got));
            assert!(lite2.process(&spk, 1, &mut want));
            assert_frame_bits_eq(&got, &want);
            audible |= got.loudness > 0.0;
            beats += usize::from(got.beat);
        }
        assert!(audible);
        assert!(beats > 1);
    }

    #[test]
    fn mini3_reset_discards_partial_fft_and_invalid_input_does_not_advance_it() {
        let mut a = SpectrumAnalyzer::new(48000, 768);
        let mut f = MusicFrame::default();
        assert!(!a.process(&[0.5; 768], 1, &mut f));
        a.reset();
        assert!(!a.process(&[0.0; 768], 1, &mut f));
        assert!(!a.process(&[0.0; 767], 1, &mut f));
        assert!(!a.process(&[0.0; 768], 0, &mut f));
        assert!(a.process(&[0.0; 768], 1, &mut f));
        assert_frame_bits_eq(&f, &MusicFrame::default());
    }

    #[test]
    fn short_or_channelless_input_is_rejected() {
        let mut a = SpectrumAnalyzer::new(48000, 1536);
        let mut f = MusicFrame::default();
        assert!(!a.process(&[0.0; 1536], 0, &mut f));
        assert!(!a.process(&[0.0; 1535], 1, &mut f));
        assert!(!a.process(&[0.0; 1536], 2, &mut f));
        assert!(a.process(&[0.0; 3072], 2, &mut f));
    }

    #[test]
    fn band_edges() {
        // 40 Hz .. 7.6 kHz log-spaced over 31.25 Hz bins (values of the C++ build).
        let a = SpectrumAnalyzer::new(16000, 512);
        assert_eq!(a.lo, [2, 3, 5, 10, 18, 34, 66, 127]);
        assert_eq!(a.hi, [2, 4, 9, 17, 33, 65, 126, 243]);
    }

    #[test]
    fn norm_is_fused_like_the_station_build() {
        // re² = 1 + 2^-11 + 2^-24 and im² = 2^-24: rounded separately both ties go to even
        // (1 + 2^-11); fused, the exact sum 1 + 2^-11 + 2^-23 is representable.
        let e = |k: i32| 2f32.powi(k);
        let z = C32 {
            re: 1.0 + e(-12),
            im: e(-12),
        };
        assert_eq!(z.norm(), 1.0 + e(-11) + e(-23));
        assert_eq!(z.re * z.re + z.im * z.im, 1.0 + e(-11));
    }
}
