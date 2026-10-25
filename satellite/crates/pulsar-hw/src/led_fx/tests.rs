//! Golden frames of the C++ renderer (`hw/led_fx.cpp` of tag rust-port-base, compiled on the
//! x86_64 host with `zig c++ -std=c++20 -O2 -mfma` and a one-off harness: clang's default
//! -ffp-contract=on then fuses the same `a * b + c` sites as the station builds). Each line of
//! tests/golden/led_fx.txt is `<kind> <params> | r g b ...` with every channel as the hex bits
//! of its f32. Effects and music are sequences: each (kind, n) starts from fresh state and its
//! frames come in order. Comparison is exact: both sides call the same glibc libm; an unfused
//! build differs on ~1/6 of the lines.

use super::*;

const EFFECTS: [&str; 11] = [
    "Rainbow",
    "Breathe",
    "Color loop",
    "Comet",
    "Twinkle",
    "Fire",
    "Aurora",
    "Lava lamp",
    "Candle",
    "Night light",
    "",
];

fn bits(s: &str) -> u32 {
    u32::from_str_radix(s, 16).expect("hex f32")
}

fn voice_of(i: i64) -> Voice {
    [
        Voice::Listening,
        Voice::Thinking,
        Voice::Speaking,
        Voice::Error,
        Voice::TimerRing,
    ][i as usize]
}

fn check(line: &str, got: &[Col], want: &[u32]) {
    assert_eq!(got.len() * 3, want.len(), "{line}: LED count");
    for (k, (c, w)) in got
        .iter()
        .flat_map(|c| [c.r, c.g, c.b])
        .zip(want)
        .enumerate()
    {
        assert_eq!(
            c.to_bits(),
            *w,
            "{line}: channel {k}: got {c}, want {}",
            f32::from_bits(*w)
        );
    }
}

#[derive(Default)]
struct Seq {
    key: Option<(String, i64, usize)>,
    fx: EffectState,
    music: MusicState,
}

impl Seq {
    /// Resets the state when a new (kind, index, n) sequence starts.
    fn enter(&mut self, kind: &str, idx: i64, n: usize) {
        let key = Some((kind.to_owned(), idx, n));
        if self.key != key {
            *self = Seq {
                key,
                ..Seq::default()
            };
        }
    }
}

#[test]
fn frames_match_the_cpp_renderer() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/golden/led_fx.txt"
    ))
    .expect("golden file");
    let mut seq = Seq::default();
    let mut lines = 0;
    for line in text.lines() {
        let (head, vals) = line.split_once(" | ").expect("separator");
        let want: Vec<u32> = vals.split(' ').map(bits).collect();
        let mut words = head.split(' ');
        let kind = words.next().unwrap();
        let p: Vec<i64> = words.map(|v| v.parse().unwrap()).collect();
        let mut out = vec![Col::default(); want.len() / 3];
        match kind {
            "voice" => voice(voice_of(p[0]), p[2], &mut out),
            "voice_logo" => voice_logo(voice_of(p[0]), p[2], &mut out),
            "timer_started" => timer_started(p[1], &mut out),
            "mute_sweep" => mute_sweep(p[0] != 0, p[2], &mut out),
            "volume" => volume(p[1] as f32 / 40.0, &mut out),
            "mute_steady" => out[0] = mute_steady(),
            "mute_logo" => out[0] = mute_logo(),
            "effect" => {
                let n = p[1] as usize;
                seq.enter(kind, p[0], n);
                let k = p[2];
                // The harness's `k * 0.1 + 1000` is contracted too.
                let t = (k as f64).mul_add(0.1, 1000.0);
                let dt = if k > 0 { 0.1 } else { 0.0 };
                let moving = effect(
                    EFFECTS[p[0] as usize],
                    rgb(255, 0, 255),
                    0.6,
                    t,
                    dt,
                    &mut seq.fx,
                    &mut out,
                );
                assert_eq!(moving, p[3] != 0, "{line}: animated");
            }
            "music" => {
                let n = p[1] as usize;
                seq.enter(kind, p[0], n);
                let k = p[2];
                let mut inp = MusicInput::default();
                for (b, v) in inp.bands.iter_mut().enumerate() {
                    *v = 0.5 + 0.45 * (((k + b as i64 * 3) % 7) - 3) as f32 / 3.0;
                }
                inp.loudness = 0.1 * (k % 11) as f32;
                inp.flash = if k % 5 == 0 { 1.0 } else { 0.3 };
                let t = k as f64 * 0.5;
                let dt = if k > 0 { 0.5 } else { 0.0 };
                music_step(&mut seq.music, &inp, k % 5 == 0, t, dt);
                music(Style::ALL[p[0] as usize], &seq.music, &inp, t, &mut out);
            }
            _ => panic!("unknown kind {kind}"),
        }
        check(line, &out, &want);
        lines += 1;
    }
    assert_eq!(lines, 1471);
}

#[test]
fn hsv_wraps_hue_in_turns() {
    let red = hsv(0.0, 1.0, 1.0);
    assert_eq!(
        red,
        Col {
            r: 1.0,
            g: 0.0,
            b: 0.0
        }
    );
    assert_eq!(hsv(1.0, 1.0, 1.0), red);
    assert_eq!(hsv(-0.5, 1.0, 1.0), hsv(0.5, 1.0, 1.0));
}
