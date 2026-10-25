//! Pure rendering of the LED ring / logo content: voice states, one-shots, effects and music
//! styles. Colours are "perceptual" 0..1 values; the LED output squares them (gamma 2).
//!
//! Strips of 12 LEDs or more are drawn like the Midi's 24-LED contour (LED 0 = home, LED n/2
//! opposite, symmetry points 0, n/4, n/2, 3n/4); shorter ones like the Lite 2's 3-LED ring. The
//! voice-state colours and timings are the ones measured on the stock patterns
//! (/system/vendor/quasar/ledpatterns/{ring,oknix}/*.led); nothing of those files is used at run
//! time.
//!
//! The arithmetic follows the C++ renderer operation by operation (f32 where it used float, f64
//! where it used double), so that frames match its golden output. clang contracts `a * b + c`
//! within one expression (-ffp-contract=on) into fused multiply-adds on both stations (aarch64
//! fmadd, armhf -mcpu=cortex_a53 vfma); those sites are `mul_add` here, everything else is
//! unfused. `mul_add` is the same single rounding: fmadd on aarch64, vfma on armhf (built with
//! `-C target-cpu=cortex-a53`, see the Makefile), a libm fma call on the x86_64 host.

use std::ops::{Add, Mul};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Col {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Mul<f32> for Col {
    type Output = Col;
    fn mul(self, k: f32) -> Col {
        Col {
            r: self.r * k,
            g: self.g * k,
            b: self.b * k,
        }
    }
}

impl Add for Col {
    type Output = Col;
    fn add(self, o: Col) -> Col {
        Col {
            r: self.r + o.r,
            g: self.g + o.g,
            b: self.b + o.b,
        }
    }
}

/// The C++ kTau = 6.2831853f is the same f32.
const TAU: f32 = std::f32::consts::TAU;

pub fn rgb(r: u8, g: u8, b: u8) -> Col {
    Col {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
    }
}

pub fn lerp(a: Col, b: Col, t: f32) -> Col {
    Col {
        r: (b.r - a.r).mul_add(t, a.r),
        g: (b.g - a.g).mul_add(t, a.g),
        b: (b.b - a.b).mul_add(t, a.b),
    }
}

pub fn cmax(a: Col, b: Col) -> Col {
    Col {
        r: a.r.max(b.r),
        g: a.g.max(b.g),
        b: a.b.max(b.b),
    }
}

/// h in turns.
pub fn hsv(h: f32, s: f32, v: f32) -> Col {
    let mut h = h % 1.0;
    if h < 0.0 {
        h += 1.0;
    }
    let i = (h * 6.0).floor();
    let f = h.mul_add(6.0, -i);
    let p = v * (1.0 - s);
    let q = v * (-f).mul_add(s, 1.0);
    let t = v * (-(1.0 - f)).mul_add(s, 1.0);
    let (r, g, b) = match (i as i32) % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    Col { r, g, b }
}

fn hsvd(deg: f32, s: f32, v: f32) -> Col {
    hsv(deg / 360.0, s, v)
}

/// Stock raw value (0xRRGGBB as written to the LEDs) at raw ratio k, in perceptual units: after
/// the gamma-2 output it is the stock value times k.
pub fn stock(rgb: u32, k: f32) -> Col {
    let k = k.max(0.0);
    let ch = |sh: u32| (((rgb >> sh) & 0xFF) as f32 / 255.0 * k).sqrt();
    Col {
        r: ch(16),
        g: ch(8),
        b: ch(0),
    }
}

#[allow(
    clippy::manual_clamp,
    reason = "NaN gives 1 as in C++ std::max(0, std::min(1, v)); f32::clamp keeps NaN"
)]
pub fn clamp01(v: f32) -> f32 {
    v.min(1.0).max(0.0)
}

pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp01((x - e0) / (e1 - e0));
    t * t * t.mul_add(-2.0, 3.0)
}

/// Smooth 0..1 oscillation with the given period, starting at minimum.
fn wave(t: f64, period: f64) -> f32 {
    (-0.5f32).mul_add((((t % period) / period) as f32 * TAU).cos(), 0.5)
}

/// Circular distance between positions a and b on a ring of n LEDs.
fn cdist(a: f32, b: f32, n: f32) -> f32 {
    let d = (a - b).abs() % n;
    d.min(n - d)
}

/// Distance (in Midi LEDs) of LED i to the nearest of the four symmetry points 0, n/4, n/2,
/// 3n/4.
fn quarter_dist(i: usize, n: usize) -> f32 {
    let q = n as f32 / 4.0;
    let r = i as f32 % q;
    r.min(q - r) * 24.0 / n as f32
}

fn xorshift(s: &mut u32) -> u32 {
    *s ^= *s << 13;
    *s ^= *s >> 17;
    *s ^= *s << 5;
    *s
}

fn hash(x: i32, y: u32) -> f32 {
    let mut h = (x as u32)
        .wrapping_mul(374_761_393)
        .wrapping_add(y.wrapping_mul(668_265_263));
    h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) & 0xFF_FFFF) as f32 / 0xFF_FFFF as f32
}

/// y is the time axis: kept in f64 so that its fraction stays exact however long the phase runs.
fn noise1(x: f32, y: f64) -> f32 {
    let fx = x.floor();
    let fy = y.floor();
    let ix = fx as i32;
    let iy = (fy as i64) as u32;
    let mut u = x - fx;
    let mut v = (y - fy) as f32;
    u = u * u * u.mul_add(-2.0, 3.0);
    v = v * v * v.mul_add(-2.0, 3.0);
    let a = hash(ix, iy);
    let b = hash(ix.wrapping_add(1), iy);
    let c = hash(ix, iy.wrapping_add(1));
    let d = hash(ix.wrapping_add(1), iy.wrapping_add(1));
    let ab = (b - a).mul_add(u, a);
    let cd = (d - c).mul_add(u, c);
    (cd - ab).mul_add(v, ab)
}

/// 2-D value noise in 0..1; y is the (long-running) time axis.
fn noise(x: f32, y: f64) -> f32 {
    noise1(x, y).mul_add(0.67, 0.33 * noise1(2.0 * x, 2.0 * y))
}

/// Rings of 12 LEDs or more are drawn like the Midi's contour, shorter ones like the Lite 2's.
pub fn wide(n: usize) -> bool {
    n >= 12
}

/// k = 0.55 + 0.45·cos(2πt/period): the stock "breathing" between ~10 % and full.
fn breathe(t: f64, period: f64) -> f32 {
    0.45f32.mul_add((TAU * ((t % period) / period) as f32).cos(), 0.55)
}

/// Listening after its 420 ms opening: 100 % <-> 40 % with a 1.54 s period.
fn listen_k(t: f32) -> f32 {
    0.30f32.mul_add((TAU * ((t - 420.0) % 1540.0 / 1540.0)).cos(), 0.70)
}

// Stock alice_* violets: Midi strip; Midi logo and Lite 2 ring. Timer green of ring / start.
const VIOLET: u32 = 0x370fff;
const VIOLET_SMALL: u32 = 0x6b0fff;
const TIMER_GREEN: u32 = 0x00dc00;
const TIMER_START_GREEN: u32 = 0x00e600;
const MUTE_RED: u32 = 0xd20000;

fn fill(out: &mut [Col], c: Col) {
    out.fill(c);
}

/// Palette hue in degrees (stock "blink" preset sequence), 15 s per entry, 3 s hue crossfade.
fn palette(t: f64) -> f32 {
    const HUES: [f32; 15] = [
        290.0, 340.0, 20.0, 355.0, 290.0, 250.0, 180.0, 250.0, 290.0, 340.0, 20.0, 48.0, 115.0,
        180.0, 250.0,
    ];
    const ENTRY: f64 = 15.0;
    const FADE: f64 = 3.0;
    let count = HUES.len();
    let cycle = t.max(0.0) % (ENTRY * count as f64);
    let i = (cycle / ENTRY) as usize;
    let u = (i as f64).mul_add(-ENTRY, cycle);
    let h0 = HUES[i];
    let h1 = HUES[(i + 1) % count];
    if u < ENTRY - FADE {
        return h0;
    }
    let dh = (h1 - h0 + 540.0) % 360.0 - 180.0;
    dh.mul_add(((u - (ENTRY - FADE)) / FADE) as f32, h0)
}

// ---------------------------------------------------------------------------------------------
// Voice states

/// Voice states (UiState minus Idle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voice {
    Listening,
    Thinking,
    Speaking,
    Error,
    TimerRing,
}

pub const ERROR_MS: i64 = 3540;

/// t in ms since the state started.
pub fn voice(v: Voice, t: i64, out: &mut [Col]) {
    let n = out.len();
    let nf = n as f32;
    let ft = t as f32;
    let half = nf / 2.0;
    match v {
        Voice::Listening => {
            if !wide(n) {
                fill(out, stock(VIOLET_SMALL, breathe(f64::from(ft), 1600.0)));
                return;
            }
            // Sweep from LED 0 both ways, meeting opposite in 420 ms; then breathe 100 % <-> 40 %.
            for (i, o) in out.iter_mut().enumerate() {
                let k = if ft < 420.0 {
                    clamp01((ft / 420.0).mul_add(half, -cdist(i as f32, 0.0, nf)) + 1.0)
                } else {
                    listen_k(ft)
                };
                *o = stock(VIOLET, k);
            }
        }
        Voice::Thinking => {
            if !wide(n) {
                // One point circling over a dim violet background.
                let c = (t % 1620) as f32 / 1620.0 * nf;
                for (i, o) in out.iter_mut().enumerate() {
                    *o = cmax(
                        stock(0x0a0219, 1.0),
                        stock(0x640eee, (1.0 - cdist(i as f32, c, nf)).max(0.0)),
                    );
                }
                return;
            }
            // One band (9 full LEDs, 3-LED tails) circling once per 1.5 s.
            let c = (t % 1500) as f32 / 1500.0 * nf;
            for (i, o) in out.iter_mut().enumerate() {
                let d = cdist(i as f32, c, nf);
                let k = if d <= 4.5 {
                    1.0
                } else if d < 7.5 {
                    ((7.5 - d) / 3.0) * ((7.5 - d) / 3.0)
                } else {
                    0.0
                };
                *o = stock(VIOLET, k);
            }
        }
        Voice::Speaking => {
            if !wide(n) {
                fill(out, stock(0x6c0fff, breathe(f64::from(ft), 1000.0)));
                return;
            }
            // Blob opposite LED 0 whose width flickers irregularly (two incommensurate periods).
            let w = 3.0f32.mul_add(
                (0.5 * (TAU * ft / 530.0).sin())
                    .mul_add(0.4f32.mul_add((TAU * ft / 1600.0).sin(), 0.6), 0.5),
                1.5,
            );
            for (i, o) in out.iter_mut().enumerate() {
                *o = stock(
                    0x7b0ee8,
                    smoothstep(w + 1.0, w - 1.0, cdist(i as f32, half, nf)),
                );
            }
        }
        Voice::Error => {
            // Three pulses: 80 ms rise, exponential fade.
            let tau = (t % 1180) as f32;
            let k = if t >= ERROR_MS {
                0.0
            } else if tau < 80.0 {
                tau / 80.0
            } else {
                (-(tau - 80.0) / 300.0).exp()
            };
            fill(out, stock(0xef0000, k));
        }
        Voice::TimerRing => {
            let k = breathe(f64::from(ft), 1300.0);
            if !wide(n) {
                for (i, o) in out.iter_mut().enumerate() {
                    *o = stock(
                        TIMER_GREEN,
                        0.45f32
                            .mul_add((TAU * ((ft % 1300.0) / 1300.0 - i as f32 / nf)).cos(), 0.55),
                    );
                }
                return;
            }
            // Two opposite blobs swaying around LEDs 2.5 and 14.5.
            let c1 = 2.5 + (TAU * ft / 1300.0).sin() * nf / 24.0;
            for (i, o) in out.iter_mut().enumerate() {
                let d = cdist(i as f32, c1, nf).min(cdist(i as f32, c1 + half, nf)) * 24.0 / nf;
                *o = stock(TIMER_GREEN, k * (1.0 - d / 2.5).max(0.0));
            }
        }
    }
}

pub fn voice_logo(v: Voice, t: i64, out: &mut [Col]) {
    let nf = out.len() as f32;
    let ft = t as f32;
    match v {
        Voice::Listening => fill(
            out,
            stock(
                VIOLET_SMALL,
                if ft < 420.0 { ft / 420.0 } else { listen_k(ft) },
            ),
        ),
        Voice::Thinking => {
            let c = (t % 1500) as f32 / 1500.0 * nf;
            for (i, o) in out.iter_mut().enumerate() {
                let d = cdist(i as f32, c, nf);
                let k = if d <= 1.0 {
                    1.0
                } else if d < 2.0 {
                    (2.0 - d) * (2.0 - d)
                } else {
                    0.0
                };
                *o = stock(VIOLET_SMALL, k);
            }
        }
        Voice::Speaking => fill(out, stock(VIOLET_SMALL, breathe(f64::from(ft), 1240.0))),
        Voice::Error => voice(v, t, out),
        Voice::TimerRing => fill(out, stock(TIMER_GREEN, breathe(f64::from(ft), 1300.0))),
    }
}

// ---------------------------------------------------------------------------------------------
// One-shots

pub const TIMER_STARTED_MS: i64 = 15000;
pub const MUTE_SWEEP_MS: i64 = 600;

pub fn timer_started(t: i64, out: &mut [Col]) {
    let n = out.len();
    let total = TIMER_STARTED_MS as f32;
    let ft = t.min(TIMER_STARTED_MS) as f32;
    if !wide(n) {
        fill(out, stock(TIMER_START_GREEN, 1.0 - ft / total));
        return;
    }
    // Green at the four symmetry points, shrinking to nothing over 15 s.
    let hw = 2.5 * (1.0 - ft / total);
    let point = if ft < 14400.0 {
        1.0
    } else {
        (total - ft) / 600.0
    };
    for (i, o) in out.iter_mut().enumerate() {
        let d = quarter_dist(i, n);
        *o = stock(
            TIMER_START_GREEN,
            if d < 0.01 {
                point
            } else {
                clamp01(hw - d + 0.5)
            },
        );
    }
}

/// Mute indication one-shot: mic muted (on) / unmuted.
pub fn mute_sweep(on: bool, t: i64, out: &mut [Col]) {
    let n = out.len();
    let nf = n as f32;
    let total = MUTE_SWEEP_MS as f32;
    let ft = t.min(MUTE_SWEEP_MS) as f32;
    if !wide(n) {
        fill(
            out,
            if on {
                stock(MUTE_RED, 1.0 - ft / total)
            } else {
                Col::default()
            },
        );
        return;
    }
    // Two red heads from LED 0 to the opposite LED (unmute: the other way), with 4-LED tails.
    let half = nf / 2.0;
    let h = (ft / 500.0).min(1.0) * half;
    let tail = if ft > 500.0 {
        (total - ft) / 100.0
    } else {
        1.0
    };
    for (i, o) in out.iter_mut().enumerate() {
        let mut d0 = cdist(i as f32, 0.0, nf);
        if !on {
            d0 = half - d0;
        }
        let k = if d0 <= h {
            clamp01(1.0 - (h - d0) / 4.0 * 24.0 / nf) * tail
        } else {
            0.0
        };
        *o = stock(MUTE_RED, k);
    }
}

/// Steady mute indication of short rings (the Midi shows it on the logo).
pub fn mute_steady() -> Col {
    stock(0x200000, 1.0)
}

pub fn mute_logo() -> Col {
    stock(0x720000, 1.0)
}

pub fn volume(v: f32, out: &mut [Col]) {
    const WIDE: [(f32, u32); 9] = [
        (0.0, 0x000d00),
        (0.067, 0x001d01),
        (0.2, 0x003d02),
        (0.33, 0x006103),
        (0.47, 0x00a002),
        (0.6, 0x00df01),
        (0.73, 0x6dda00),
        (0.87, 0xf44100),
        (1.0, 0xf80100),
    ];
    const SHORT: [(f32, u32); 7] = [
        (0.0, 0x116111),
        (1.0 / 6.0, 0x3f9f25),
        (1.0 / 3.0, 0x71d50e),
        (0.5, 0xc9fa00),
        (2.0 / 3.0, 0xecbf00),
        (5.0 / 6.0, 0xec6a00),
        (1.0, 0xf80000),
    ];
    let n = out.len();
    let v = clamp01(v);
    let pts: &[(f32, u32)] = if wide(n) { &WIDE } else { &SHORT };
    // Per-channel linear interpolation of the raw values.
    let mut j = 1;
    while j < pts.len() - 1 && v > pts[j].0 {
        j += 1;
    }
    let f = clamp01((v - pts[j - 1].0) / (pts[j].0 - pts[j - 1].0));
    let mut raw = 0u32;
    for sh in [16, 8, 0] {
        let a = ((pts[j - 1].1 >> sh) & 0xFF) as f32;
        let b = ((pts[j].1 >> sh) & 0xFF) as f32;
        raw |= ((b - a).mul_add(f, a).round() as u32) << sh;
    }
    if !wide(n) {
        fill(out, stock(raw, 1.0));
        return;
    }
    // Grows from the four symmetry points.
    let h = 3.5 * v;
    for (i, o) in out.iter_mut().enumerate() {
        let d = quarter_dist(i, n);
        *o = stock(raw, if d < 0.01 { 1.0 } else { clamp01(h - d + 0.5) });
    }
}

// ---------------------------------------------------------------------------------------------
// Effects

/// Per-layer state of the stateful effects (Twinkle).
#[derive(Clone, Debug)]
pub struct EffectState {
    rng: u32,
    /// Per LED, seconds since its sparkle started; < 0 idle.
    age: Vec<f32>,
}

impl Default for EffectState {
    fn default() -> Self {
        Self {
            rng: 0x9E37_79B9,
            age: Vec::new(),
        }
    }
}

/// t: effect phase in s (speed-scaled), dt: its step since the previous frame. bri scales the
/// result. Returns true if the content moves (needs further frames).
pub fn effect(
    name: &str,
    c: Col,
    bri: f32,
    t: f64,
    dt: f64,
    st: &mut EffectState,
    out: &mut [Col],
) -> bool {
    let n = out.len();
    let nf = n as f32;
    match name {
        "Rainbow" => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = hsv(((t / 6.0) % 1.0) as f32 + i as f32 / nf, 1.0, 1.0) * bri;
            }
            true
        }
        "Breathe" => {
            fill(out, c * (bri * 0.9f32.mul_add(wave(t, 4.0), 0.1)));
            true
        }
        "Color loop" => {
            fill(out, hsv(((t / 20.0) % 1.0) as f32, 1.0, 1.0) * bri);
            true
        }
        "Comet" => {
            let head = ((t / 2.0) % 1.0) as f32 * nf;
            let len = (0.4 * nf).max(1.5);
            for (i, o) in out.iter_mut().enumerate() {
                // Distance behind the head.
                let d = 2.0f32.mul_add(nf, head - i as f32) % nf;
                let k = if d < len {
                    (1.0 - d / len) * (1.0 - d / len)
                } else {
                    0.0
                };
                *o = c * (bri * k);
            }
            true
        }
        "Twinkle" => {
            if st.age.len() != n {
                st.age = vec![-1.0; n];
            }
            // Sparkles per LED per second.
            let rate = if n <= 3 { 0.8 } else { 0.6 };
            for (a, o) in st.age.iter_mut().zip(out.iter_mut()) {
                if *a >= 0.0 {
                    *a += dt as f32;
                    if *a >= 0.95 {
                        *a = -1.0;
                    }
                } else if f64::from(xorshift(&mut st.rng)) / 4_294_967_296.0 < rate * dt {
                    *a = 0.0;
                }
                let a = *a;
                let env = if a < 0.0 {
                    0.0
                } else if a < 0.15 {
                    a / 0.15
                } else {
                    (-3.0 * (a - 0.15) / 0.8).exp()
                };
                *o = c * (bri * env.max(0.08));
            }
            true
        }
        "Fire" => {
            for (i, o) in out.iter_mut().enumerate() {
                let heat = 0.65f32.mul_add(noise(0.35 * i as f32, 2.5 * t), 0.35);
                let col = if heat < 0.33 {
                    stock(0xff0000, heat / 0.33)
                } else if heat < 0.66 {
                    lerp(
                        stock(0xff0000, 1.0),
                        stock(0xff6000, 1.0),
                        (heat - 0.33) / 0.33,
                    )
                } else {
                    lerp(
                        stock(0xff6000, 1.0),
                        stock(0xffb000, 1.0),
                        (heat - 0.66) / 0.34,
                    )
                };
                *o = col * bri;
            }
            true
        }
        "Aurora" => {
            for (i, o) in out.iter_mut().enumerate() {
                let x = i as f32 / nf;
                let s1 = (TAU * (x + ((t / 17.0) % 1.0) as f32)).sin();
                let s2 = (TAU * 2.0f32.mul_add(x, -(((t / 11.0) % 1.0) as f32))).sin();
                let hue = 40.0f32.mul_add(s2, 55.0f32.mul_add(s1, 165.0));
                let v = 0.65f32.mul_add(noise(0.2 * i as f32, 0.3 * t), 0.35);
                *o = hsvd(hue, 0.9, v) * bri;
            }
            true
        }
        "Lava lamp" => {
            const COL: [u32; 4] = [0xfe3200, 0xfe0065, 0x5800fe, 0x00fe1e];
            const PHASE: [f64; 4] = [0.0, 0.25, 0.5, 0.75];
            const VEL: [f64; 4] = [0.013, -0.021, 0.017, -0.029];
            const SIGMA: [f32; 4] = [0.12, 0.10, 0.14, 0.09];
            for (i, o) in out.iter_mut().enumerate() {
                let mut sum = Col::default();
                let mut nearest = Col::default();
                let mut wsum = 0.0f32;
                let mut dmin = 1e9f32;
                for j in 0..4 {
                    let centre = (VEL[j].mul_add(t, PHASE[j]) % 1.0) as f32 * nf;
                    let d = cdist(i as f32, centre + nf, nf);
                    let s = (nf * SIGMA[j]).max(0.6);
                    let w = (-d * d / (2.0 * s * s)).exp();
                    sum = sum + stock(COL[j], 1.0) * w;
                    wsum += w;
                    if d < dmin {
                        dmin = d;
                        nearest = stock(COL[j], 1.0);
                    }
                }
                *o = cmax(sum * (1.0 / wsum.max(1.0)), nearest * 0.05) * bri;
            }
            true
        }
        "Candle" => {
            let g = 0.04f32.mul_add(
                noise(7.0, 8.0 * t),
                0.08f32.mul_add(noise(0.0, 0.6 * t), 0.88),
            );
            for (i, o) in out.iter_mut().enumerate() {
                let wave = (TAU * i as f32 / 6.0 + ((0.9 * t) % f64::from(TAU)) as f32).cos();
                let v = (-0.08f32).mul_add(0.5f32.mul_add(wave, 0.5), 1.0);
                *o = stock(0xffb200, g * v) * bri;
            }
            true
        }
        "Night light" => {
            fill(out, stock(0xd4ffbf, 1.0) * bri);
            false
        }
        _ => {
            fill(out, c * bri);
            false
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Music

/// Music styles, without "Off" / "All styles" (resolved by the compositor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Spectrum,
    Pulse,
    Disco,
    Polar,
}

impl Style {
    pub const ALL: [Style; 4] = [Style::Spectrum, Style::Pulse, Style::Disco, Style::Polar];
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MusicInput {
    pub bands: [f32; 8],
    pub loudness: f32,
    /// 1 on a beat, decaying.
    pub flash: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MusicState {
    /// Disco rotation, turns.
    theta: f64,
    /// Disco beat impulse, turns/s.
    impulse: f64,
    /// Polar centre / target, turns.
    p: f64,
    target: f64,
    /// Polar: phase of the next target pick.
    next_pick: f64,
    rng: u32,
}

impl Default for MusicState {
    fn default() -> Self {
        Self {
            theta: 0.0,
            impulse: 0.0,
            p: 0.0,
            target: 0.0,
            next_pick: 0.0,
            rng: 0x2545_F491,
        }
    }
}

/// Advances the moving styles by phase step dt (speed-scaled seconds) at phase t.
pub fn music_step(st: &mut MusicState, inp: &MusicInput, beat: bool, t: f64, dt: f64) {
    if beat {
        st.impulse += 1.0;
    }
    st.impulse *= (-dt / 0.4).exp();
    let speed = 0.6f64.mul_add(f64::from(inp.loudness), 0.15) + st.impulse;
    st.theta = speed.mul_add(dt, st.theta) % 1.0;
    if t >= st.next_pick {
        st.target = f64::from(xorshift(&mut st.rng)) / 4_294_967_296.0;
        st.next_pick = t + 5.0;
    }
    // Shortest way, turns.
    let diff = (st.target - st.p + 1.5) % 1.0 - 0.5;
    let step = 0.1 * dt;
    let mv = if diff.abs() <= step {
        diff
    } else {
        step.copysign(diff)
    };
    st.p = (st.p + mv + 1.0) % 1.0;
}

pub fn music(s: Style, st: &MusicState, inp: &MusicInput, t: f64, out: &mut [Col]) {
    let n = out.len();
    let nf = n as f32;
    let hue = palette(t);
    let loud = clamp01(inp.loudness);
    let b = &inp.bands;
    match s {
        Style::Spectrum => {
            if !wide(n) {
                let v = [
                    (b[0] + b[1] + b[2]) / 3.0,
                    (b[3] + b[4] + b[5]) / 3.0,
                    (b[6] + b[7]) / 2.0,
                ];
                for (i, o) in out.iter_mut().enumerate() {
                    *o = hsvd(60.0f32.mul_add((i % 3) as f32, hue), 1.0, v[i % 3]);
                }
                return;
            }
            // Bass at LED 0, highs opposite, mirrored on both sides.
            for (i, o) in out.iter_mut().enumerate() {
                let d0 = i.min(n - i);
                let band = 7.min(d0 * 8 / (n / 2 + 1));
                *o = hsvd(25.0f32.mul_add(band as f32, hue), 1.0, b[band]);
            }
        }
        Style::Pulse => {
            let v = 0.3f32
                .mul_add(inp.flash, 0.85f32.mul_add(loud, 0.15))
                .min(1.0);
            if !wide(n) {
                fill(out, hsvd(hue, 0.9, v));
                return;
            }
            let w = 1.5 + loud * 10.5 * nf / 24.0;
            for (i, o) in out.iter_mut().enumerate() {
                *o = hsvd(
                    hue,
                    0.9,
                    v * clamp01(w - cdist(i as f32, nf / 2.0, nf) + 0.5),
                );
            }
        }
        Style::Disco => {
            let v = 0.75f32.mul_add(loud, 0.25);
            let reach = if wide(n) { 2.0 } else { 1.0 };
            for (i, o) in out.iter_mut().enumerate() {
                let mut c = Col::default();
                for j in 0..3 {
                    let pos = ((st.theta + f64::from(j) / 3.0) % 1.0) as f32 * nf;
                    c = cmax(
                        c,
                        hsvd(
                            120.0f32.mul_add(j as f32, hue),
                            1.0,
                            v * clamp01(reach - cdist(i as f32, pos, nf)),
                        ),
                    );
                }
                *o = c;
            }
        }
        Style::Polar => {
            let bg = hsvd(hue + 180.0, 0.8, 0.12);
            let half = nf / 2.0;
            let p = st.p as f32 * nf;
            for (i, o) in out.iter_mut().enumerate() {
                let d = cdist(i as f32, p, nf);
                let band = 7.min((d * 8.0 / (half + 1.0)) as usize);
                *o = cmax(bg, hsvd(hue, 1.0, b[band] * (1.0 - 0.5 * d / half)));
            }
        }
    }
}

#[cfg(test)]
mod tests;
