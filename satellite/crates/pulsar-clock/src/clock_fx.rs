//! Pure rendering of the 7-segment clock animations: the stock voice / ringing / player / boot
//! patterns and the music level meter. The stock envelopes were measured from the Lite 2's
//! /system/vendor/quasar/ledpatterns/clock/*.binscled.zst (the Midi ships only ringing, the
//! same); nothing of those files is used at run time.

/// Brightness 0..255: `seg[digit][segment]`, digits 0..3 left to right, segments a..g = 0..6;
/// `colon` = both colon dots; `center` = the dot between them at the height of g (Lite 2 "DM").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub seg: [[u8; 7]; 4],
    pub colon: u8,
    pub center: u8,
}

/// The first four loop, the others play once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anim {
    Listening,
    Thinking,
    Speaking,
    Ringing,
    Boot,
    Pause,
    Next,
    Previous,
}

const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const D: usize = 3;
const E: usize = 4;
const F: usize = 5;
const G: usize = 6;

/// (t ms, value): piecewise linear; two keys with the same t make a step.
type Key = (i64, u8);

// The C++ reference was built by clang with -ffp-contract=on for CPUs with FMA (aarch64,
// cortex-a53): every `a + b * c` inside one expression became a fused multiply-add, which
// rounds differently at .5 boundaries (e.g. SPEAKING at 1330 ms). The same expressions use
// `mul_add` here.

/// C `lround` for the values drawn here (half away from zero).
fn round_u8(v: f64) -> u8 {
    v.round() as u8
}

fn at(k: &[Key], t: i64) -> u8 {
    if t < k[0].0 {
        return k[0].1;
    }
    let mut i = 0;
    while i + 1 < k.len() && k[i + 1].0 <= t {
        i += 1;
    }
    if i + 1 >= k.len() {
        return k[i].1;
    }
    let x = (t - k[i].0) as f64 / (k[i + 1].0 - k[i].0) as f64;
    let (v0, v1) = (f64::from(k[i].1), f64::from(k[i + 1].1));
    round_u8((v1 - v0).mul_add(x, v0))
}

// Envelopes measured on the stock patterns (<= 10/255 off). Outer = digits 0 and 3, inner = 1
// and 2.
#[rustfmt::skip]
const LISTEN_OUTER: &[Key] = &[(0, 0), (20, 36), (40, 198), (60, 223), (180, 215),
    (200, 192), (260, 54), (280, 26), (320, 4), (1000, 0)];
#[rustfmt::skip]
const LISTEN_INNER: &[Key] = &[(0, 0), (60, 6), (80, 81), (100, 182), (120, 218), (140, 223),
    (340, 222), (400, 208), (460, 174), (580, 70), (700, 14), (1000, 0)];
#[rustfmt::skip]
const LISTEN_CENTER: &[Key] = &[(0, 0), (80, 0), (180, 74), (300, 102), (760, 81), (920, 50),
    (980, 23), (1000, 0)];

#[rustfmt::skip]
const THINK_G0: &[Key] = &[(0, 25), (100, 47), (200, 159), (220, 150), (280, 11), (300, 0),
    (1180, 1), (1200, 17), (1260, 145), (1300, 165), (1540, 48), (1800, 25)];
#[rustfmt::skip]
const THINK_G1: &[Key] = &[(0, 3), (140, 14), (180, 59), (220, 157), (240, 169), (260, 149),
    (300, 49), (340, 5), (1120, 3), (1160, 23), (1200, 129), (1220, 166),
    (1240, 174), (1280, 140), (1340, 50), (1380, 25), (1500, 6), (1800, 3)];
#[rustfmt::skip]
const THINK_CENTER: &[Key] = &[(0, 3), (180, 7), (200, 22), (260, 171), (280, 173), (300, 151),
    (360, 29), (400, 5), (1080, 4), (1120, 26), (1140, 61), (1180, 162),
    (1200, 171), (1220, 155), (1280, 33), (1320, 8), (1800, 3)];
#[rustfmt::skip]
const THINK_G2: &[Key] = &[(0, 3), (220, 6), (240, 23), (280, 136), (300, 168), (320, 174),
    (360, 139), (420, 47), (500, 10), (1040, 8), (1080, 34), (1160, 159),
    (1180, 149), (1220, 54), (1240, 19), (1260, 5), (1800, 3)];
#[rustfmt::skip]
const THINK_G3: &[Key] = &[(0, 0), (260, 2), (280, 21), (340, 148), (380, 167), (580, 47),
    (680, 25), (960, 19), (1020, 45), (1140, 144), (1160, 121), (1200, 15),
    (1220, 1), (1800, 0)];

#[rustfmt::skip]
const SPEAK_OUTER: &[Key] = &[(0, 0), (800, 5), (860, 38), (960, 168), (1000, 194),
    (1040, 163), (1100, 46), (1140, 4), (1340, 0)];
#[rustfmt::skip]
const SPEAK_INNER: &[Key] = &[(0, 0), (140, 4), (220, 62), (300, 84), (360, 78), (500, 18),
    (660, 7), (840, 127), (1000, 204), (1080, 163), (1280, 9), (1340, 0)];
#[rustfmt::skip]
const SPEAK_CENTER: &[Key] = &[(0, 9), (100, 40), (140, 74), (180, 129), (280, 180), (360, 168),
    (520, 53), (640, 19), (1000, 204), (1040, 189), (1240, 51), (1340, 9)];

// Eyes on digits 0 and 3: g is the eye, a/b/f the lids; the right eye winks late.
const BOOT_EYE: &[Key] = &[(0, 64), (60, 255), (4880, 255)];
#[rustfmt::skip]
const BOOT_LID_LEFT: &[Key] = &[(0, 0), (60, 0), (140, 255), (1040, 255), (1040, 0), (1220, 0),
    (1220, 255), (1400, 255), (1400, 0), (1560, 0), (1560, 255), (4880, 255)];
#[rustfmt::skip]
const BOOT_LID_RIGHT: &[Key] = &[(0, 0), (60, 0), (140, 255), (1040, 255), (1040, 0),
    (1220, 0), (1220, 255), (1400, 255), (1400, 0), (1560, 0),
    (1560, 255), (3800, 255), (3800, 0), (4220, 0), (4220, 255),
    (4880, 255)];

const PAUSE: &[Key] = &[(0, 0), (280, 255), (1960, 255), (2200, 0)];

/// Vertical half-digit bar: (digit, right half b+c / left half e+f).
type Bar = (usize, bool);

fn bar(f: &mut Frame, (digit, right): Bar, v: u8) {
    let d = &mut f.seg[digit];
    if right {
        d[B] = v;
        d[C] = v;
    } else {
        d[E] = v;
        d[F] = v;
    }
}

/// Computed in double, handed on as float (as the reference did).
fn ease(x: f64) -> f32 {
    ((1.0 - (std::f64::consts::PI * x).cos()) / 2.0) as f32
}

/// Ringing: the bars light pairwise from the centre outwards, each ramping up, holding and
/// down.
fn ring_bar(u: i64) -> u8 {
    if !(0..800).contains(&u) {
        return 0;
    }
    let u = u as f64;
    let v: f32 = if u < 160.0 {
        128.0 * ease(u / 160.0)
    } else if u < 320.0 {
        127.0f32.mul_add(ease((u - 160.0) / 160.0), 128.0)
    } else if u < 480.0 {
        255.0
    } else if u < 640.0 {
        (-127.0f32).mul_add(ease((u - 480.0) / 160.0), 255.0)
    } else {
        (-128.0f32).mul_add(ease((u - 640.0) / 160.0), 128.0)
    };
    round_u8(f64::from(v))
}

/// Next / previous: a column sweep, each column flashing on and fading out.
fn sweep_column(c: i64, t: i64) -> u8 {
    let on = 20 + 160 * c;
    if t < on {
        0
    } else if t < on + 160 {
        255
    } else if t < on + 480 {
        round_u8(128.0 - 128.0 * (t - on - 160) as f64 / 320.0)
    } else {
        0
    }
}

pub fn loops(a: Anim) -> bool {
    matches!(
        a,
        Anim::Listening | Anim::Thinking | Anim::Speaking | Anim::Ringing
    )
}

/// Loop period / one-shot length.
pub fn duration_ms(a: Anim) -> i64 {
    match a {
        Anim::Listening => 1000,
        Anim::Thinking => 1800,
        Anim::Speaking => 1340,
        Anim::Ringing => 1780,
        Anim::Boot => 4880,
        Anim::Pause => 2200,
        Anim::Next | Anim::Previous => 1500,
    }
}

/// Frame at `t` ms: loops wrap t, one-shots draw nothing at t >= duration.
pub fn render(a: Anim, t: i64) -> Frame {
    let mut f = Frame::default();
    let dur = duration_ms(a);
    let mut t = t.max(0);
    if loops(a) {
        t %= dur;
    } else if t >= dur {
        return f;
    }
    match a {
        Anim::Listening => {
            let (o, i) = (at(LISTEN_OUTER, t), at(LISTEN_INNER, t));
            (f.seg[0][G], f.seg[3][G], f.seg[1][G], f.seg[2][G]) = (o, o, i, i);
            f.center = at(LISTEN_CENTER, t);
        }
        Anim::Thinking => {
            f.seg[0][G] = at(THINK_G0, t);
            f.seg[1][G] = at(THINK_G1, t);
            f.center = at(THINK_CENTER, t);
            f.seg[2][G] = at(THINK_G2, t);
            f.seg[3][G] = at(THINK_G3, t);
        }
        Anim::Speaking => {
            let (o, i) = (at(SPEAK_OUTER, t), at(SPEAK_INNER, t));
            (f.seg[0][G], f.seg[3][G], f.seg[1][G], f.seg[2][G]) = (o, o, i, i);
            f.center = at(SPEAK_CENTER, t);
        }
        Anim::Ringing => {
            const BARS: [[Bar; 2]; 4] = [
                [(1, true), (2, false)],
                [(1, false), (2, true)],
                [(0, true), (3, false)],
                [(0, false), (3, true)],
            ];
            for (k, pair) in (0..).zip(BARS) {
                let v = ring_bar(t - (480 + 160 * k));
                bar(&mut f, pair[0], v);
                bar(&mut f, pair[1], v);
            }
        }
        Anim::Boot => {
            let eye = at(BOOT_EYE, t);
            f.seg[0][G] = eye;
            f.seg[3][G] = eye;
            let (l, r) = (at(BOOT_LID_LEFT, t), at(BOOT_LID_RIGHT, t));
            for s in [A, B, F] {
                f.seg[0][s] = l;
                f.seg[3][s] = r;
            }
        }
        Anim::Pause => {
            let v = at(PAUSE, t);
            bar(&mut f, (1, true), v);
            bar(&mut f, (2, false), v);
        }
        Anim::Next | Anim::Previous => {
            const NEXT: [Bar; 4] = [(2, false), (2, true), (3, false), (3, true)];
            const PREV: [Bar; 4] = [(1, true), (1, false), (0, true), (0, false)];
            let cols = if a == Anim::Next { NEXT } else { PREV };
            for (c, col) in (0..).zip(cols) {
                bar(&mut f, col, sweep_column(c, t));
            }
        }
    }
    f
}

/// C++ `std::clamp`: `v` itself when it is NaN; no panic on `lo > hi`.
pub fn clamp(v: f32, lo: f32, hi: f32) -> f32 {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}

/// "Levels" music style: one meter per digit (0..1), lit d -> g -> a bottom up.
pub fn levels(digits: &[f32; 4]) -> Frame {
    let mut f = Frame::default();
    let lit = |v: f32| (255.0 * clamp(v, 0.0, 1.0)).round() as u8;
    for (seg, &v) in f.seg.iter_mut().zip(digits) {
        let x = 3.0 * v;
        seg[D] = lit(x);
        seg[G] = lit(x - 1.0);
        seg[A] = lit(x - 2.0);
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANIMS: [(&str, Anim); 8] = [
        ("LISTENING", Anim::Listening),
        ("THINKING", Anim::Thinking),
        ("SPEAKING", Anim::Speaking),
        ("RINGING", Anim::Ringing),
        ("BOOT", Anim::Boot),
        ("PAUSE", Anim::Pause),
        ("NEXT", Anim::Next),
        ("PREVIOUS", Anim::Previous),
    ];

    /// Sparse frame as in the golden files: "<index>=<value>" of the non-zero LEDs, comma
    /// separated; index = digit * 7 + segment, 28 = colon, 29 = centre; "-" when dark.
    fn sparse(f: &Frame) -> String {
        let mut v: Vec<u8> = f.seg.iter().flatten().copied().collect();
        v.extend([f.colon, f.center]);
        let parts: Vec<String> = v
            .iter()
            .enumerate()
            .filter(|(_, x)| **x != 0)
            .map(|(i, x)| format!("{i}={x}"))
            .collect();
        if parts.is_empty() {
            "-".into()
        } else {
            parts.join(",")
        }
    }

    /// Every animation on a 5 ms grid (one-shots from before their start to past their end,
    /// loops over more than one period), compared with the C++ `clockfx::render`.
    #[test]
    fn render_matches_cxx_golden() {
        let golden = include_str!("../tests/golden/clock_fx_render.txt");
        let mut n = 0;
        for line in golden.lines() {
            let mut it = line.split(' ');
            let (name, t, want) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
            let anim = ANIMS.iter().find(|(n, _)| *n == name).unwrap().1;
            let t: i64 = t.parse().unwrap();
            assert_eq!(sparse(&render(anim, t)), want, "{name} t={t}");
            n += 1;
        }
        assert!(n > 3000, "golden file truncated: {n} lines");
    }

    #[test]
    fn levels_match_cxx_golden() {
        let golden = include_str!("../tests/golden/clock_fx_levels.txt");
        let mut n = 0;
        for line in golden.lines() {
            let mut it = line.split(' ');
            let digits: Vec<f32> = (0..4)
                .map(|_| it.next().unwrap().parse().unwrap())
                .collect();
            let want = it.next().unwrap();
            let f = levels(&[digits[0], digits[1], digits[2], digits[3]]);
            assert_eq!(sparse(&f), want, "{line}");
            n += 1;
        }
        assert!(n >= 100, "golden file truncated: {n} lines");
    }

    #[test]
    fn durations_and_looping() {
        for (_, a) in ANIMS {
            let looping = matches!(
                a,
                Anim::Listening | Anim::Thinking | Anim::Speaking | Anim::Ringing
            );
            assert_eq!(loops(a), looping);
            if !looping {
                assert_eq!(render(a, duration_ms(a)), Frame::default());
            } else {
                assert_eq!(render(a, 37), render(a, 37 + 3 * duration_ms(a)));
            }
        }
    }
}
