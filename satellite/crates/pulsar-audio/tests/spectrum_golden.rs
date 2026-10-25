//! SpectrumAnalyzer against the C++ analyzer (tests/golden/spectrum.txt): a sine scenario
//! (three tones and noise, then a fade-out), silence (with a quiet tone fading through the
//! silence threshold) and a beat sequence (80 Hz kicks, strong, weak and one too early), on the
//! Lite 2 layout (48 kHz, chunk 1536, 1 speaker channel) and the Midi one (96 kHz, chunk 3072,
//! 4 channels). The inputs are integer samples, generated here exactly as by the C++ generator.

use std::f64::consts::PI;

use pulsar_audio::SpectrumAnalyzer;
use pulsar_core::MusicFrame;

const GOLDEN: &str = include_str!("golden/spectrum.txt");

struct Layout {
    name: &'static str,
    rate: u32,
    chunk: usize,
    nspk: usize,
}

const LAYOUTS: [Layout; 2] = [
    Layout {
        name: "lite2",
        rate: 48000,
        chunk: 1536,
        nspk: 1,
    },
    Layout {
        name: "midi",
        rate: 96000,
        chunk: 3072,
        nspk: 4,
    },
];
const SCENARIOS: [(&str, usize); 3] = [("sine", 60), ("silence", 40), ("beats", 128)];

fn sat(v: i64) -> i16 {
    v.clamp(-32768, 32767) as i16
}

/// The C++ generator's LCG: uniform in [-range, range).
struct Noise(u32);

impl Noise {
    fn next(&mut self, range: i64) -> i64 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
        i64::from((self.0 >> 8) % (2 * range) as u32) - range
    }
}

fn tone(amp: f64, f: f64, rate: u32, n: u64) -> i64 {
    let w = 2.0 * PI * f / f64::from(rate);
    (amp * (w * n as f64).sin()).round_ties_even() as i64
}

fn sample(
    scenario: usize,
    g: &mut Noise,
    rate: u32,
    n: u64,
    chunk: usize,
    c: usize,
    nspk: usize,
) -> i16 {
    let scale = (c + 1) as f64 / nspk as f64;
    match scenario {
        0 => {
            let mut v = tone(16384.0 * scale, 1000.0, rate, n)
                + tone(3000.0 * scale, 150.0, rate, n)
                + tone(500.0 * scale, 4000.0, rate, n)
                + g.next(100);
            if chunk >= 30 {
                v >>= (chunk - 30) / 5;
            }
            sat(v)
        }
        1 => {
            if (20..32).contains(&chunk) {
                sat(tone(400.0 * scale / (chunk - 19) as f64, 700.0, rate, n))
            } else {
                0
            }
        }
        _ => {
            let mut v = tone(1000.0 * scale, 2000.0, rate, n) + g.next(200);
            if chunk % 16 < 2 || chunk % 32 == 20 {
                let amp = if (chunk / 16) % 2 == 1 {
                    6000.0
                } else {
                    20000.0
                };
                v += tone(amp * scale, 80.0, rate, n);
            }
            sat(v)
        }
    }
}

#[test]
fn matches_the_cxx_analyzer() {
    let mut golden = GOLDEN.lines().filter(|l| !l.starts_with('#'));
    let mut max_diff = 0f32;
    let mut beats = 0;
    for l in &LAYOUTS {
        for (s, &(name, chunks)) in SCENARIOS.iter().enumerate() {
            let mut a = SpectrumAnalyzer::new(l.rate, l.chunk);
            assert!(a.ok());
            let mut g = Noise(1);
            let mut spk = vec![0f32; l.chunk * l.nspk];
            for k in 0..chunks {
                for i in 0..l.chunk {
                    for c in 0..l.nspk {
                        let n = (k * l.chunk + i) as u64;
                        let v = sample(s, &mut g, l.rate, n, k, c, l.nspk);
                        spk[i * l.nspk + c] = f32::from(v) * (1.0 / 32768.0);
                    }
                }
                let mut f = MusicFrame::default();
                let filled = a.process(&spk, l.nspk, &mut f);

                let line = golden.next().expect("golden data ends early");
                let at = format!("{} {name} chunk {k}", l.name);
                let cols: Vec<&str> = line.split_whitespace().collect();
                assert_eq!(cols.len(), 14, "{at}: {line}");
                assert_eq!(cols[..3], [l.name, name, &k.to_string()], "{at}: {line}");
                assert_eq!(filled, cols[3] == "1", "{at}: filled");
                assert_eq!(f.beat, cols[4] == "1", "{at}: beat");
                beats += u32::from(f.beat);
                let want: Vec<f32> = cols[5..].iter().map(|v| v.parse().unwrap()).collect();
                let got = [f.loudness].into_iter().chain(f.bands);
                for (j, (w, v)) in want.iter().zip(got).enumerate() {
                    let d = (w - v).abs();
                    // Bit-exact on the host the data was made on (glibc 2.39); another glibc's
                    // sinf / cosf / log10f may differ by an ulp.
                    assert!(d <= 1e-6, "{at}: column {j}: {v} vs C++ {w}");
                    max_diff = max_diff.max(d);
                }
            }
        }
    }
    assert!(golden.next().is_none(), "golden data has more chunks");
    assert!(beats > 10, "{beats} beats");
    eprintln!("max difference to the C++ analyzer: {max_diff:e}, {beats} beats");
}
