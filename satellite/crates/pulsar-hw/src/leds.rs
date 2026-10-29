//! LED ring (+ logo) of the Station Midi (24 + 6 RGB) and Lite 2 (3 RGB, no logo); layout per
//! Model.
//!
//! How the hardware is driven: the LEDs sit behind kernel LED triggers that expose them as
//! framebuffers (Midi: "ledfb-ring"/"ledfb-logo" -> /dev/fb0 "ring" 24x1, /dev/fb1 "logo" 6x1;
//! Lite 2: "led_ring_fb" -> /dev/fb0 "YNDX LED ring" 3x1), 24 bpp, rgba 8/16,8/8,8/0 -> bytes per
//! pixel are B,G,R; pixel i = the i-th sysfs LED of the strip.
//! On the Midi a pwrite() is only shown on the next write or FBIOPAN_DISPLAY (the ledfb driver
//! pushes the framebuffer at the *start* of each write/pan), so every frame is pwrite() +
//! FBIOPAN_DISPLAY. Cost ~2.5 ms per ring update (i2c), far cheaper than per-LED sysfs writes.
//! If the framebuffers are missing or the triggers are not active, the per-channel sysfs
//! brightness files are kept open instead.
//!
//! Ring index order: Yandex patterns (ledpatterns/ring/*.led, one RRGGBB column per LED) anchor
//! their animations at the first LED and grow symmetrically to its neighbours (mute.led,
//! alice_thinking.led), so LED 0 is the "home" position; all animations are relative to it and
//! go towards increasing indices.
//!
//! Case colour: the stock firmware corrects the LED colour for the case fabric with a 3x3 matrix
//! per colour (/system/vendor/quasar/ledconfigs/<colour>.json, chosen by the Yandex backend's
//! device_color, which is not stored on the device).
//!
//! Ring content, first active wins: volume overlay > voice state (Error / TimerRing always,
//! Listening / Thinking / Speaking with voice indication on) > one-shots (timer started, mute) >
//! led_show > mute (Lite 2: steady dim red) > user light > music visualization > off. The Midi
//! logo shows the voice state, else red while muted, else the ring's average colour.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd as _;

use log::{error, info, warn};
use pulsar_core::model::{LedStrip, Model};
use pulsar_core::{AmbientDimmer, MUSIC_STALE_MS, MusicFrame, SENDSPIN_FRESH_MS, Timers, UiState};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::led_fx::{self as fx, Col};
use crate::sys::{Framebuffer, I2c};

/// ~30 fps while animating (stock 20 ms frames resampled by time).
const FRAME_MS: u64 = 33;
/// Global cap: never drive LEDs above 80 % PWM.
const MAX_SCALE: f32 = 0.8;
const VOLUME_SHOW_MS: i64 = 1500;
/// Crossfade between layers.
const FADE_MS: i64 = 250;
/// Longest time step of one frame (after an idle stretch).
const MAX_STEP_MS: i64 = 100;
/// "All styles": each style this long, crossfading into the next.
const CYCLE_STYLE_MS: i64 = 30000;
const CYCLE_FADE_MS: i64 = 1000;
/// Rate limit for repeated per-frame write warnings.
const WARN_EVERY_MS: i64 = 10000;
const LED_DIR: &str = "/sys/class/leds/";
const CASE_DIR: &str = "/system/vendor/quasar/ledconfigs/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

const EFFECT_NAMES: [&str; 10] = [
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
];

/// After "Off" and "All styles", in `fx::Style` order.
const MUSIC_STYLES: [&str; 6] = ["Off", "All styles", "Spectrum", "Pulse", "Disco", "Polar"];

/// Effects of the user light and `show_temporary` ("" = solid colour).
pub fn effect_names() -> &'static [&'static str] {
    &EFFECT_NAMES
}

pub fn music_styles() -> &'static [&'static str] {
    &MUSIC_STYLES
}

/// Sorted stems of /system/vendor/quasar/ledconfigs/*.json.
pub fn case_colors() -> Vec<String> {
    case_colors_in(CASE_DIR)
}

fn case_colors_in(dir: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            (name.len() > 5 && name.ends_with(".json")).then(|| name[..name.len() - 5].to_owned())
        })
        .collect();
    out.sort();
    out
}

type Matrix = [[f32; 3]; 3];

fn load_matrix(dir: &str, stem: &str) -> Option<Matrix> {
    parse_matrix(&std::fs::read_to_string(format!("{dir}{stem}.json")).ok()?)
}

/// Stock format: {"type": "matrix", "matrix": [[r0], [r1], [r2]]}; the key counts, not the type
/// value. The first nine numbers after the key's '[' (up to a '}') are the rows.
fn parse_matrix(text: &str) -> Option<Matrix> {
    const KEY: &str = "\"matrix\"";
    let bytes = text.as_bytes();
    let mut pos = 0;
    let open = loop {
        let found = pos + text[pos..].find(KEY)?;
        let after = found + KEY.len();
        let colon = bytes[after..]
            .iter()
            .position(|b| !b" \t\r\n".contains(b))
            .map(|i| after + i);
        if let Some(c) = colon
            && bytes[c] == b':'
        {
            break c + text[c..].find('[')?;
        }
        pos = after;
    };
    let mut m = [0f32; 9];
    let mut got = 0;
    let mut p = open;
    while p < bytes.len() && bytes[p] != 0 && bytes[p] != b'}' && got < 9 {
        if bytes[p].is_ascii_digit() || bytes[p] == b'-' || bytes[p] == b'.' {
            let (v, len) = strtod_prefix(&bytes[p..])?;
            if !v.is_finite() {
                return None;
            }
            m[got] = v as f32;
            got += 1;
            p += len;
        } else {
            p += 1;
        }
    }
    if got < 9 {
        return None;
    }
    Some([[m[0], m[1], m[2]], [m[3], m[4], m[5]], [m[6], m[7], m[8]]])
}

/// The number strtod(3) reads at the start of `s` in the C locale, and its length; None when
/// it reads nothing. Hex mantissas are exact up to 13 digits (more never occur in these files).
fn strtod_prefix(s: &[u8]) -> Option<(f64, usize)> {
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    let starts =
        |i: usize, w: &[u8]| s.len() >= i + w.len() && s[i..i + w.len()].eq_ignore_ascii_case(w);
    let mut i = 0;
    let neg = matches!(at(0), b'-');
    if matches!(at(0), b'-' | b'+') {
        i = 1;
    }
    let sign = if neg { -1.0 } else { 1.0 };
    if starts(i, b"inf") {
        let len = if starts(i, b"infinity") { 8 } else { 3 };
        return Some((sign * f64::INFINITY, i + len));
    }
    if starts(i, b"nan") {
        let mut end = i + 3;
        if at(end) == b'(' {
            let close = s[end + 1..]
                .iter()
                .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .map(|k| end + 1 + k);
            if let Some(c) = close
                && s[c] == b')'
            {
                end = c + 1;
            }
        }
        return Some((f64::NAN, end));
    }
    if at(i) == b'0'
        && matches!(at(i + 1), b'x' | b'X')
        && (at(i + 2).is_ascii_hexdigit() || (at(i + 2) == b'.' && at(i + 3).is_ascii_hexdigit()))
    {
        let mut j = i + 2;
        let mut mant = 0f64;
        let mut scale = 0i32;
        while at(j).is_ascii_hexdigit() {
            mant = mant * 16.0 + f64::from((at(j) as char).to_digit(16)?);
            j += 1;
        }
        if at(j) == b'.' {
            j += 1;
            while at(j).is_ascii_hexdigit() {
                mant = mant * 16.0 + f64::from((at(j) as char).to_digit(16)?);
                scale -= 4;
                j += 1;
            }
        }
        if matches!(at(j), b'p' | b'P') {
            let mut k = j + 1;
            let eneg = at(k) == b'-';
            if matches!(at(k), b'-' | b'+') {
                k += 1;
            }
            if at(k).is_ascii_digit() {
                let mut e = 0i32;
                while at(k).is_ascii_digit() {
                    e = e.saturating_mul(10).saturating_add(i32::from(at(k) - b'0'));
                    k += 1;
                }
                scale = scale.saturating_add(if eneg { -e } else { e });
                j = k;
            }
        }
        return Some((sign * mant * 2f64.powi(scale), j));
    }
    let start = i;
    let mut digits = 0;
    while at(i).is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if at(i) == b'.' {
        i += 1;
        while at(i).is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(at(i), b'e' | b'E') {
        let mut k = i + 1;
        if matches!(at(k), b'-' | b'+') {
            k += 1;
        }
        if at(k).is_ascii_digit() {
            while at(k).is_ascii_digit() {
                k += 1;
            }
            i = k;
        }
    }
    let v: f64 = std::str::from_utf8(&s[start..i]).ok()?.parse().ok()?;
    Some((sign * v, i))
}

enum Cmd {
    UiState(UiState),
    Muted(bool),
    UserLight {
        on: bool,
        color: Rgb,
        brightness: f32,
        effect: String,
        transition_ms: u32,
    },
    Volume(f32),
    AmbientLux(f32),
    TimerStarted,
    ShowTemporary {
        effect: String,
        color: Rgb,
        seconds: i32,
    },
    ClearTemporary,
    VoiceIndication(bool),
    Speed(f32),
    MusicStyle(String),
    MediaPlaying(bool),
    Music(MusicFrame, bool),
    CaseMatrix(Option<Matrix>),
}

/// Commands to the LED actor; the ring goes dark when the last handle is dropped.
#[derive(Clone)]
pub struct LedsHandle {
    tx: UnboundedSender<Cmd>,
}

impl LedsHandle {
    fn send(&self, cmd: Cmd) {
        // The actor only stops after every handle is gone.
        let _ = self.tx.send(cmd);
    }

    pub fn set_ui_state(&self, s: UiState) {
        self.send(Cmd::UiState(s));
    }

    /// Mic mute indication; the first call only sets the state (no one-shot).
    pub fn set_muted(&self, muted: bool) {
        self.send(Cmd::Muted(muted));
    }

    /// User light (HA light entity). Changes fade over transition_ms (0 = cut).
    pub fn set_user_light(
        &self,
        on: bool,
        color: Rgb,
        brightness: f32,
        effect: String,
        transition_ms: u32,
    ) {
        self.send(Cmd::UserLight {
            on,
            color,
            brightness,
            effect,
            transition_ms,
        });
    }

    /// Transient volume indicator (0..1) shown on the ring for 1.5 s.
    pub fn show_volume(&self, v: f32) {
        self.send(Cmd::Volume(v));
    }

    /// Ambient light: dims the ring and logo (smoothed, with hysteresis; floor keeps them
    /// visible).
    pub fn set_ambient_lux(&self, lux: f32) {
        self.send(Cmd::AmbientLux(lux));
    }

    /// 15 s one-shot.
    pub fn show_timer_started(&self) {
        self.send(Cmd::TimerStarted);
    }

    /// Effect (one of effect_names(), "" = solid colour) at full brightness for seconds;
    /// seconds <= 0: 5 s, capped at 3600 s. Replaces the previous one.
    pub fn show_temporary(&self, effect: String, color: Rgb, seconds: i32) {
        self.send(Cmd::ShowTemporary {
            effect,
            color,
            seconds,
        });
    }

    pub fn clear_temporary(&self) {
        self.send(Cmd::ClearTemporary);
    }

    pub fn set_voice_indication(&self, on: bool) {
        self.send(Cmd::VoiceIndication(on));
    }

    /// 0.25..4: motion of effects and music styles.
    pub fn set_speed(&self, k: f32) {
        self.send(Cmd::Speed(k));
    }

    /// One of music_styles(); anything else is ignored.
    pub fn set_music_style(&self, style: String) {
        self.send(Cmd::MusicStyle(style));
    }

    pub fn set_media_playing(&self, playing: bool) {
        self.send(Cmd::MediaPlaying(playing));
    }

    /// Music features, ~30 per second while playing. Sendspin frames take precedence over the
    /// speaker reference while they keep coming.
    pub fn music_frame(&self, f: MusicFrame, from_sendspin: bool) {
        self.send(Cmd::Music(f, from_sendspin));
    }

    /// Colour correction for the case fabric: a stem of case_colors(), "" = none. Loaded here,
    /// synchronously; false when the file is missing or unreadable (no correction then).
    pub fn set_case_color(&self, stem: &str) -> bool {
        let (matrix, ok) = if stem.is_empty() {
            (None, true)
        } else if let Some(m) = load_matrix(CASE_DIR, stem) {
            info!("leds: case colour {stem}");
            (Some(m), true)
        } else {
            warn!("leds: case colour {stem} unreadable");
            (None, false)
        };
        self.send(Cmd::CaseMatrix(matrix));
        ok
    }
}

/// Opens the framebuffers (or the sysfs fallback) and starts the actor. Without either the
/// output is a no-op ("LEDs unavailable"); state and case colour still work.
pub fn spawn_leds(model: &'static Model) -> LedsHandle {
    let n = usize::try_from(model.ring.count).unwrap_or(0);
    let nlogo = usize::try_from(model.logo.count).unwrap_or(0);
    let sink = if !model.ring.i2c_dev.is_empty() {
        match open_i2c(model, n, nlogo) {
            Some(sink) => {
                info!(
                    "leds: driving {n} ring LEDs via i2c {}@0x{:02x}",
                    model.ring.i2c_dev, model.ring.i2c_addr
                );
                sink
            }
            None => Sink::None,
        }
    } else if let Some(fb) = open_fb(model, nlogo) {
        info!("leds: driving {n} ring + {nlogo} logo LEDs via framebuffer");
        fb
    } else if let Some(files) = open_sysfs(model, n, nlogo) {
        info!("leds: driving {n} ring + {nlogo} logo LEDs via sysfs brightness");
        Sink::Sysfs(files)
    } else {
        warn!("LEDs unavailable");
        Sink::None
    };
    let opened = !matches!(sink, Sink::None);
    let (tx, rx) = unbounded_channel();
    let comp = Compositor::new(n, nlogo);
    let out = Output::new(sink, n, nlogo);
    pulsar_core::actors::spawn_actor("leds", run(comp, out, rx, opened));
    LedsHandle { tx }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TimerKey {
    /// Repeating while the content moves.
    Frame,
    /// One-shot at the next end of timed content while nothing moves.
    Expiry,
    /// A write failed on a static frame: retry it even though nothing changes.
    Retry,
}

/// Retry period of a failed static frame (the I²C ring is the only sink that reports errors).
const RETRY_MS: u64 = 1000;

async fn run(mut comp: Compositor, mut out: Output, mut rx: UnboundedReceiver<Cmd>, opened: bool) {
    let epoch = Instant::now();
    let now = || i64::try_from(epoch.elapsed().as_millis()).unwrap_or(i64::MAX);
    let mut timers = Timers::new();
    if opened {
        comp.state_t0 = now();
        frame(&mut comp, &mut out, &mut timers, now());
    }
    loop {
        tokio::select! {
            biased;
            _ = timers.next() => frame(&mut comp, &mut out, &mut timers, now()),
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                let t = now();
                if matches!(cmd, Cmd::CaseMatrix(_)) {
                    // Rewrite every LED with the new correction.
                    out.forget_last();
                }
                if comp.apply(cmd, t) {
                    frame(&mut comp, &mut out, &mut timers, t);
                }
            }
        }
    }
    // Do not leave the last frame lit after the daemon exits.
    out.clear(now());
}

fn frame(comp: &mut Compositor, out: &mut Output, timers: &mut Timers<TimerKey>, now: i64) {
    let animated = comp.frame(now);
    let failed = out.write(&comp.px, now);
    if animated {
        timers.cancel(TimerKey::Expiry);
        timers.cancel(TimerKey::Retry);
        if !timers.is_set(TimerKey::Frame) {
            timers.set_repeating(TimerKey::Frame, FRAME_MS, FRAME_MS);
        }
        return;
    }
    timers.cancel(TimerKey::Frame);
    timers.cancel(TimerKey::Expiry);
    if failed {
        // Nothing will change the pixels any more; a dropped frame would stay dark until the
        // next command. Retry while the sink keeps failing.
        timers.set(TimerKey::Retry, RETRY_MS);
    } else {
        timers.cancel(TimerKey::Retry);
    }
    if let Some(e) = comp.next_expiry(now) {
        timers.set(TimerKey::Expiry, u64::try_from(e - now).unwrap_or(0));
    }
}

// -------------------------------------------------------------------------------------------------
// Output

#[allow(
    clippy::large_enum_variant,
    reason = "one per process; the framebuffer variant is the one in use"
)]
enum Sink {
    None,
    Fb {
        ring: Framebuffer,
        logo: Option<Framebuffer>,
    },
    /// IS31FL3236-compatible 18-channel controller (6 RGB pixels).
    I2c {
        dev: I2c,
        addr: u16,
    },
    /// [ring r,g,b x n][logo r,g,b x nlogo]
    Sysfs(Vec<File>),
    /// Host tests: (time, pixels) of every write that changed something.
    #[cfg(test)]
    Capture(Captured),
}

/// (time, pixels) of the writes a test sink saw.
#[cfg(test)]
type Captured = std::rc::Rc<std::cell::RefCell<Vec<(i64, Vec<u8>)>>>;

#[derive(Default)]
struct WriteWarn {
    last: Option<i64>,
    suppressed: u32,
}

struct Output {
    sink: Sink,
    n: usize,
    /// Last written pixels (RGB); `has_last` false = rewrite everything.
    last: Vec<u8>,
    has_last: bool,
    bgr: Vec<u8>,
    /// fb0 / fb1 write warnings.
    warn: [WriteWarn; 2],
}

impl Output {
    fn new(sink: Sink, n: usize, nlogo: usize) -> Self {
        Self {
            sink,
            n,
            last: vec![0; (n + nlogo) * 3],
            has_last: false,
            bgr: Vec::with_capacity(n.max(nlogo) * 3),
            warn: Default::default(),
        }
    }

    fn forget_last(&mut self) {
        self.has_last = false;
    }

    fn clear(&mut self, now: i64) {
        let off = vec![0u8; self.last.len()];
        self.has_last = false;
        self.write(&off, now);
    }

    /// Writes changed pixels; true when a sink reported an error and the frame should be retried.
    fn write(&mut self, px: &[u8], now: i64) -> bool {
        let split = self.n * 3;
        let have_last = self.has_last;
        let mut failed = false;
        match &mut self.sink {
            Sink::None => {}
            #[cfg(test)]
            Sink::Capture(frames) => {
                if !have_last || px != &self.last[..] {
                    frames.borrow_mut().push((now, px.to_vec()));
                }
            }
            Sink::Fb { ring, logo } => {
                if !have_last || px[..split] != self.last[..split] {
                    failed |= !write_fb(
                        ring,
                        &px[..split],
                        &mut self.bgr,
                        &mut self.warn[0],
                        "ring fb",
                        now,
                    );
                }
                if let Some(logo) = logo
                    && (!have_last || px[split..] != self.last[split..])
                {
                    failed |= !write_fb(
                        logo,
                        &px[split..],
                        &mut self.bgr,
                        &mut self.warn[1],
                        "logo fb",
                        now,
                    );
                }
            }
            Sink::I2c { dev, addr } => {
                if !have_last || px[..split] != self.last[..split] {
                    let mut bgr = [0u8; 18];
                    for (i, p) in px[..split].as_chunks::<3>().0.iter().enumerate() {
                        bgr[i * 3..i * 3 + 3].copy_from_slice(&[p[2], p[1], p[0]]);
                    }
                    // 0x01: first PWM register; 0x25 = 0xAA latches the frame.
                    let r = dev
                        .write_register(*addr, 0x01, &bgr)
                        .and_then(|()| dev.write_register(*addr, 0x25, &[0xaa]));
                    if let Err(e) = r {
                        warn_write(&mut self.warn[0], "i2c", &e, now);
                        // Keep `last` as it was: the retry must send these pixels again.
                        return true;
                    }
                }
            }
            Sink::Sysfs(files) => {
                for (k, f) in files.iter().enumerate() {
                    if !have_last || px[k] != self.last[k] {
                        pulsar_core::sysfs::write_int_fd(f.as_raw_fd(), i32::from(px[k]));
                    }
                }
            }
        }
        if failed {
            // Keep `last` as it was, so the retry does not skip these pixels.
            return true;
        }
        self.last.copy_from_slice(px);
        self.has_last = true;
        false
    }
}

/// Writes RGB pixels to a framebuffer as BGR and latches them.
fn write_fb(
    fb: &mut Framebuffer,
    px: &[u8],
    bgr: &mut Vec<u8>,
    w: &mut WriteWarn,
    what: &str,
    now: i64,
) -> bool {
    bgr.clear();
    for p in px.as_chunks::<3>().0 {
        bgr.extend_from_slice(&[p[2], p[1], p[0]]);
    }
    let ok = match fb.write(bgr) {
        Ok(k) if k == bgr.len() => true,
        Ok(_) => {
            warn_write(w, what, &io::Error::from(io::ErrorKind::WriteZero), now);
            false
        }
        Err(e) => {
            warn_write(w, what, &e, now);
            false
        }
    };
    fb.pan();
    ok
}

fn warn_write(w: &mut WriteWarn, what: &str, err: &io::Error, now: i64) {
    if let Some(t) = w.last
        && now - t < WARN_EVERY_MS
    {
        w.suppressed += 1;
        return;
    }
    warn!(
        "leds: {what} write: {} ({} similar suppressed)",
        crate::errstr(err),
        w.suppressed
    );
    w.last = Some(now);
    w.suppressed = 0;
}

fn led_name(model: &Model, s: &LedStrip, color: &str, i: i32) -> String {
    format!(
        "{}:{color}:{}{}",
        model.led_prefix,
        s.sysfs_kind,
        s.first_index + i
    )
}

fn trigger_is(led: &str, trig: &str) -> bool {
    pulsar_core::sysfs::read_file(&format!("{LED_DIR}{led}/trigger"))
        .is_some_and(|t| t.contains(&format!("[{trig}]")))
}

/// Opens one strip's framebuffer if its trigger is active and the fb has the expected name.
fn open_strip_fb(model: &Model, s: &LedStrip) -> Option<Framebuffer> {
    if s.fb_dev.is_empty() {
        return None; // sysfs-only ring (Mini 2)
    }
    let red0 = led_name(model, s, "red", 0);
    if !trigger_is(&red0, s.trigger) {
        warn!("leds: trigger {} not active on {red0}", s.trigger);
        return None;
    }
    let dev = s.fb_dev.rsplit('/').next().unwrap_or(s.fb_dev);
    let name = pulsar_core::sysfs::read_file(&format!("/sys/class/graphics/{dev}/name"))
        .unwrap_or_default();
    let name = pulsar_core::sysfs::trim(&name);
    if name != s.fb_name {
        warn!("leds: {} is '{name}', expected '{}'", s.fb_dev, s.fb_name);
        return None;
    }
    match Framebuffer::open(s.fb_dev) {
        Ok(fb) => Some(fb),
        Err(e) => {
            warn!("leds: open {}: {}", s.fb_dev, crate::errstr(&e));
            None
        }
    }
}

/// IS31FL3236-compatible controller: enable the output stage, set every channel's current and
/// clear the PWM registers; the stock firmware sends the same three writes.
fn open_i2c(model: &Model, n: usize, nlogo: usize) -> Option<Sink> {
    if n != 6 || nlogo != 0 {
        warn!("leds: i2c backend expects 6 ring LEDs and no logo ({n} + {nlogo})");
        return None;
    }
    let dev = match I2c::open(model.ring.i2c_dev) {
        Ok(dev) => dev,
        Err(e) => {
            warn!("leds: open {}: {}", model.ring.i2c_dev, crate::errstr(&e));
            return None;
        }
    };
    let addr = model.ring.i2c_addr;
    let imax = [3u8; 18];
    if let Err(e) = dev
        .write_register(addr, 0x00, &[1])
        .and_then(|()| dev.write_register(addr, 0x26, &imax))
        .and_then(|()| dev.write_register(addr, 0x4b, &[1]))
    {
        warn!(
            "leds: init {}@0x{addr:02x}: {}",
            model.ring.i2c_dev,
            crate::errstr(&e)
        );
        return None;
    }
    Some(Sink::I2c { dev, addr })
}

fn open_fb(model: &Model, nlogo: usize) -> Option<Sink> {
    let ring = open_strip_fb(model, &model.ring);
    let logo = if nlogo > 0 {
        open_strip_fb(model, &model.logo)
    } else {
        None
    };
    let ring = ring?;
    if nlogo > 0 && logo.is_none() {
        return None;
    }
    Some(Sink::Fb { ring, logo })
}

fn open_sysfs(model: &Model, n: usize, nlogo: usize) -> Option<Vec<File>> {
    let mut files = Vec::with_capacity((n + nlogo) * 3);
    for i in 0..n + nlogo {
        for c in ["red", "green", "blue"] {
            let name = if i < n {
                led_name(model, &model.ring, c, i as i32)
            } else {
                led_name(model, &model.logo, c, (i - n) as i32)
            };
            let path = format!("{LED_DIR}{name}/brightness");
            match std::fs::OpenOptions::new().write(true).open(&path) {
                Ok(f) => files.push(f),
                Err(_) => {
                    error!("leds: cannot open {path}");
                    return None;
                }
            }
        }
    }
    Some(files)
}

// -------------------------------------------------------------------------------------------------
// Composition

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    /// Before the first frame.
    None,
    Volume,
    Voice,
    OneShot,
    Show,
    Mute,
    User,
    Music,
    Off,
}

/// One-shots: the mute sweep shows over the timer-started countdown, which goes on underneath.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sweep {
    None,
    MuteOn,
    MuteOff,
}

/// For the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    None,
    Speaker,
    Sendspin,
}

fn to_voice(s: UiState) -> fx::Voice {
    match s {
        UiState::Listening => fx::Voice::Listening,
        UiState::Thinking => fx::Voice::Thinking,
        UiState::Speaking => fx::Voice::Speaking,
        UiState::Error => fx::Voice::Error,
        // Idle is never drawn: voice_shown() is false.
        UiState::TimerRing | UiState::Idle => fx::Voice::TimerRing,
    }
}

fn to_col(c: Rgb) -> Col {
    fx::rgb(c.r, c.g, c.b)
}

/// Gamma 2 (perceived brightness roughly linear in the colour values), ambient dimming `d` (on
/// the perceptual scale), case matrix (hue kept when it would clip), 80 % cap.
fn to_px(c: Col, d: f32, matrix: Option<&Matrix>) -> [u8; 3] {
    let mut p = [
        fx::clamp01(c.r) * d,
        fx::clamp01(c.g) * d,
        fx::clamp01(c.b) * d,
    ];
    for v in &mut p {
        *v *= *v;
    }
    if let Some(m) = matrix {
        let mut q = [0f32; 3];
        for (r, q) in q.iter_mut().enumerate() {
            // Fused as clang contracts it on the stations: m0·p0 + m1·p1 first, then + m2·p2.
            *q = m[r][2]
                .mul_add(p[2], m[r][0].mul_add(p[0], m[r][1] * p[1]))
                .max(0.0);
        }
        let mx = q[0].max(q[1]).max(q[2]);
        for r in 0..3 {
            p[r] = if mx > 1.0 { q[r] / mx } else { q[r] };
        }
    }
    p.map(|v| (v.min(1.0) * MAX_SCALE * 255.0).round() as u8)
}

/// Layer state and rendering; `frame(now)` leaves the pixels to write in `px`.
struct Compositor {
    n: usize,
    nlogo: usize,
    /// Lite 2: no logo, mute shown on the ring.
    short_ring: bool,
    matrix: Option<Matrix>,

    // voice state
    state: UiState,
    state_t0: i64,
    voice_indication: bool,
    // one-shots
    sweep: Sweep,
    sweep_t0: i64,
    timer_shot: bool,
    timer_t0: i64,
    muted: bool,
    mute_known: bool,
    // led_show
    show_on: bool,
    show_effect: String,
    show_col: Col,
    show_until: i64,
    show_fx: fx::EffectState,
    // user light
    user_on: bool,
    user_col: Col,
    user_bri: f32,
    user_effect: String,
    user_fx: fx::EffectState,
    user_changed: bool,
    user_transition: u32,
    // music
    /// Index into MUSIC_STYLES: 0 Off, 1 All styles, then fx::Style + 2.
    style: usize,
    media: bool,
    /// Latest frame of the source in use; None = never.
    mus_in: MusicFrame,
    mus_in_ms: Option<i64>,
    /// Beat in a frame not yet drawn.
    mus_beat: bool,
    sendspin_until: i64,
    source: Source,
    mus_shown: fx::MusicInput,
    mus_st: fx::MusicState,
    cycle_t0: i64,
    // volume overlay
    volume: f32,
    volume_until: i64,
    // time
    speed: f32,
    /// Speed-scaled seconds.
    phase: f64,
    last_ms: Option<i64>,
    // composition
    layer: Layer,
    /// Ring + logo.
    shown: Vec<Col>,
    from: Vec<Col>,
    target: Vec<Col>,
    /// Next music style ("All styles").
    alt: Vec<Col>,
    fade_t0: i64,
    fade_ms: i64,
    dimmer: AmbientDimmer,
    px: Vec<u8>,
}

impl Compositor {
    fn new(n: usize, nlogo: usize) -> Self {
        let all = n + nlogo;
        Self {
            n,
            nlogo,
            short_ring: !fx::wide(n),
            matrix: None,
            state: UiState::Idle,
            state_t0: 0,
            voice_indication: true,
            sweep: Sweep::None,
            sweep_t0: 0,
            timer_shot: false,
            timer_t0: 0,
            muted: false,
            mute_known: false,
            show_on: false,
            show_effect: String::new(),
            show_col: Col::default(),
            show_until: 0,
            show_fx: fx::EffectState::default(),
            user_on: false,
            user_col: fx::rgb(255, 255, 255),
            user_bri: 1.0,
            user_effect: String::new(),
            user_fx: fx::EffectState::default(),
            user_changed: false,
            user_transition: 0,
            style: 0,
            media: false,
            mus_in: MusicFrame::default(),
            mus_in_ms: None,
            mus_beat: false,
            sendspin_until: 0,
            source: Source::None,
            mus_shown: fx::MusicInput::default(),
            mus_st: fx::MusicState::default(),
            cycle_t0: 0,
            volume: 0.0,
            volume_until: 0,
            speed: 1.0,
            phase: 0.0,
            last_ms: None,
            layer: Layer::None,
            shown: vec![Col::default(); all],
            from: vec![Col::default(); all],
            target: vec![Col::default(); all],
            alt: vec![Col::default(); n],
            fade_t0: 0,
            fade_ms: 0,
            dimmer: AmbientDimmer::new(),
            px: vec![0; all * 3],
        }
    }

    /// Applies a command at `now`; true if a frame is due at once.
    fn apply(&mut self, cmd: Cmd, now: i64) -> bool {
        match cmd {
            Cmd::UiState(s) => {
                if s == self.state {
                    return false;
                }
                // The error indication runs to its end; only another active state replaces it.
                if self.state == UiState::Error
                    && s == UiState::Idle
                    && now - self.state_t0 < fx::ERROR_MS
                {
                    return false;
                }
                self.set_state(s, now);
            }
            Cmd::Muted(muted) => {
                if self.mute_known && muted == self.muted {
                    return false;
                }
                let first = !self.mute_known;
                self.mute_known = true;
                self.muted = muted;
                if !first && (muted || !self.short_ring) {
                    self.sweep = if muted { Sweep::MuteOn } else { Sweep::MuteOff };
                    self.sweep_t0 = now;
                } else if !first && self.sweep == Sweep::MuteOn {
                    // Lite 2 unmute: no sweep, the flash goes with the mute.
                    self.sweep = Sweep::None;
                }
            }
            Cmd::UserLight {
                on,
                color,
                brightness,
                effect,
                transition_ms,
            } => {
                self.user_on = on;
                self.user_col = to_col(color);
                self.user_bri = fx::clamp01(brightness);
                self.user_effect = effect;
                self.user_changed = true;
                self.user_transition = transition_ms;
            }
            Cmd::Volume(v) => {
                self.volume = fx::clamp01(v);
                self.volume_until = now + VOLUME_SHOW_MS;
            }
            Cmd::AmbientLux(lux) => return self.dimmer.update(lux),
            Cmd::TimerStarted => {
                self.timer_shot = true;
                self.timer_t0 = now;
            }
            Cmd::ShowTemporary {
                effect,
                color,
                seconds,
            } => {
                self.show_on = true;
                self.show_effect = effect;
                self.show_col = to_col(color);
                let seconds = if seconds <= 0 { 5 } else { seconds.min(3600) };
                self.show_until = now + i64::from(seconds) * 1000;
                self.show_fx = fx::EffectState::default();
            }
            Cmd::ClearTemporary => {
                if !self.show_on {
                    return false;
                }
                self.show_on = false;
            }
            Cmd::VoiceIndication(on) => {
                if on == self.voice_indication {
                    return false;
                }
                self.voice_indication = on;
            }
            Cmd::Speed(k) => {
                self.speed = if k.is_finite() {
                    k.clamp(0.25, 4.0)
                } else {
                    1.0
                };
                return false;
            }
            Cmd::MusicStyle(style) => {
                let Some(i) = MUSIC_STYLES.iter().position(|s| *s == style) else {
                    return false;
                };
                if i == self.style {
                    return false;
                }
                self.style = i;
                self.cycle_t0 = now;
            }
            Cmd::MediaPlaying(playing) => {
                if playing == self.media {
                    return false;
                }
                self.media = playing;
                if playing {
                    self.cycle_t0 = now;
                    self.mus_shown = fx::MusicInput::default();
                } else {
                    self.source = Source::None;
                }
            }
            Cmd::Music(f, from_sendspin) => {
                if from_sendspin {
                    self.sendspin_until = now + SENDSPIN_FRESH_MS;
                } else if now < self.sendspin_until {
                    return false;
                }
                let src = if from_sendspin {
                    Source::Sendspin
                } else {
                    Source::Speaker
                };
                if src != self.source && self.media {
                    self.source = src;
                    info!(
                        "leds: music source {}",
                        if from_sendspin { "sendspin" } else { "speaker" }
                    );
                }
                self.mus_in = f;
                self.mus_in_ms = Some(now);
                if f.beat {
                    self.mus_beat = true;
                }
                return false;
            }
            Cmd::CaseMatrix(m) => self.matrix = m,
        }
        true
    }

    fn set_state(&mut self, s: UiState, now: i64) {
        self.state = s;
        self.state_t0 = now;
    }

    /// Ring/logo brightness factor from ambient light: 0.3 in the dark .. 1.
    fn dim(&self) -> f32 {
        0.3 + 0.7 * (self.dimmer.factor() - 0.02) / 0.98
    }

    fn voice_shown(&self) -> bool {
        match self.state {
            UiState::Idle => false,
            UiState::Error | UiState::TimerRing => true,
            _ => self.voice_indication,
        }
    }

    fn music_shown(&self) -> bool {
        self.media && self.style != 0
    }

    /// Smoothed music inputs: peaks held and released over 250 ms, beat flash over 150 ms.
    fn update_music(&mut self, now: i64, dt_s: f64) {
        let fresh = self.mus_in_ms.is_some_and(|t| now - t < MUSIC_STALE_MS);
        let rel = (-dt_s / 0.25).exp() as f32;
        let shown = &mut self.mus_shown;
        for (s, v) in shown.bands.iter_mut().zip(self.mus_in.bands) {
            *s = (if fresh { v } else { 0.0 }).max(*s * rel);
        }
        shown.loudness = (if fresh { self.mus_in.loudness } else { 0.0 }).max(shown.loudness * rel);
        shown.flash *= (-dt_s / 0.15).exp() as f32;
        if self.mus_beat {
            shown.flash = 1.0;
        }
    }

    fn render_style(&self, s: fx::Style, out: &mut [Col]) {
        fx::music(s, &self.mus_st, &self.mus_shown, self.phase, out);
    }

    fn render_music(&mut self, now: i64, out: &mut [Col]) {
        if self.style >= 2 {
            return self.render_style(fx::Style::ALL[self.style - 2], out);
        }
        // All styles: Spectrum -> Pulse -> Disco -> Polar, 30 s each on the wall clock, 1 s
        // crossfade.
        let t = now - self.cycle_t0;
        let i = (t / CYCLE_STYLE_MS % 4) as usize;
        self.render_style(fx::Style::ALL[i], out);
        let into = t % CYCLE_STYLE_MS;
        if into < CYCLE_STYLE_MS - CYCLE_FADE_MS {
            return;
        }
        let mut alt = std::mem::take(&mut self.alt);
        self.render_style(fx::Style::ALL[(i + 1) % 4], &mut alt);
        let f = fx::smoothstep(
            0.0,
            1.0,
            (into - (CYCLE_STYLE_MS - CYCLE_FADE_MS)) as f32 / CYCLE_FADE_MS as f32,
        );
        for (o, a) in out.iter_mut().zip(&alt) {
            *o = fx::lerp(*o, *a, f);
        }
        self.alt = alt;
    }

    /// Draws `lay` into `target` (ring + logo); returns true if it moves.
    fn render(&mut self, now: i64, dt: f64, lay: Layer, target: &mut [Col]) -> bool {
        target.fill(Col::default());
        let (ring, logo) = target.split_at_mut(self.n);
        let mut animated = true;
        match lay {
            Layer::Volume => {
                fx::volume(self.volume, ring);
                animated = false;
            }
            Layer::Voice => fx::voice(to_voice(self.state), now - self.state_t0, ring),
            Layer::OneShot => {
                if self.sweep == Sweep::None {
                    fx::timer_started(now - self.timer_t0, ring);
                } else {
                    fx::mute_sweep(self.sweep == Sweep::MuteOn, now - self.sweep_t0, ring);
                }
            }
            Layer::Show => {
                animated = fx::effect(
                    &self.show_effect,
                    self.show_col,
                    1.0,
                    self.phase,
                    dt,
                    &mut self.show_fx,
                    ring,
                );
            }
            Layer::Mute => {
                ring.fill(fx::mute_steady());
                animated = false;
            }
            Layer::User => {
                animated = fx::effect(
                    &self.user_effect,
                    self.user_col,
                    self.user_bri,
                    self.phase,
                    dt,
                    &mut self.user_fx,
                    ring,
                );
            }
            Layer::Music => self.render_music(now, ring),
            Layer::Off | Layer::None => animated = false,
        }
        if self.nlogo > 0 {
            if self.voice_shown() {
                fx::voice_logo(to_voice(self.state), now - self.state_t0, logo);
                animated = true;
            } else if self.muted {
                logo.fill(fx::mute_logo());
            } else {
                let k = 1.0 / self.n as f32;
                let avg = ring.iter().fold(Col::default(), |a, c| a + *c * k);
                logo.fill(avg);
            }
        }
        animated
    }

    fn active_layer(&self, now: i64) -> Layer {
        if now < self.volume_until {
            Layer::Volume
        } else if self.voice_shown() {
            Layer::Voice
        } else if self.sweep != Sweep::None || self.timer_shot {
            Layer::OneShot
        } else if self.show_on {
            Layer::Show
        } else if self.short_ring && self.muted {
            Layer::Mute
        } else if self.user_on && self.user_bri > 0.0 {
            Layer::User
        } else if self.music_shown() {
            Layer::Music
        } else {
            Layer::Off
        }
    }

    /// Ends timed content that ran out.
    fn expire(&mut self, now: i64) {
        if self.state == UiState::Error && now - self.state_t0 >= fx::ERROR_MS {
            self.set_state(UiState::Idle, now);
        }
        if self.sweep != Sweep::None && now - self.sweep_t0 >= fx::MUTE_SWEEP_MS {
            self.sweep = Sweep::None;
        }
        if self.timer_shot && now - self.timer_t0 >= fx::TIMER_STARTED_MS {
            self.timer_shot = false;
        }
        if self.show_on && now >= self.show_until {
            self.show_on = false;
        }
    }

    /// Earliest pending expiry after now.
    fn next_expiry(&self, now: i64) -> Option<i64> {
        let mut t: Option<i64> = None;
        let mut take = |e: i64| {
            if e > now && t.is_none_or(|t| e < t) {
                t = Some(e);
            }
        };
        take(self.volume_until);
        if self.state == UiState::Error {
            take(self.state_t0 + fx::ERROR_MS);
        }
        if self.sweep != Sweep::None {
            take(self.sweep_t0 + fx::MUTE_SWEEP_MS);
        }
        if self.timer_shot {
            take(self.timer_t0 + fx::TIMER_STARTED_MS);
        }
        if self.show_on {
            take(self.show_until);
        }
        t
    }

    /// Renders the frame at `now` into `px`; true if the content moves.
    fn frame(&mut self, now: i64) -> bool {
        let step = self.last_ms.map_or(0, |l| (now - l).clamp(0, MAX_STEP_MS));
        self.last_ms = Some(now);
        let dt = step as f64 / 1000.0 * f64::from(self.speed);
        self.phase += dt;
        self.expire(now);

        let lay = self.active_layer(now);
        if lay == Layer::Music {
            self.update_music(now, step as f64 / 1000.0);
            fx::music_step(
                &mut self.mus_st,
                &self.mus_shown,
                self.mus_beat,
                self.phase,
                dt,
            );
        }
        self.mus_beat = false;
        let mut target = std::mem::take(&mut self.target);
        let mut animated = self.render(now, dt, lay, &mut target);
        self.target = target;

        // Fades: between layers 250 ms (the volume overlay appears at once), user light changes
        // over their transition.
        let mut fade = None;
        if lay != self.layer {
            fade = Some(if lay == Layer::Volume { 0 } else { FADE_MS });
        }
        if self.user_changed
            && (lay == Layer::User || (self.layer == Layer::User && lay != self.layer))
        {
            fade = Some(i64::from(self.user_transition));
        }
        self.user_changed = false;
        self.layer = lay;
        if let Some(f) = fade {
            self.from.copy_from_slice(&self.shown);
            self.fade_t0 = now;
            self.fade_ms = f;
        }
        if self.fade_ms > 0 && now - self.fade_t0 < self.fade_ms {
            let f = fx::smoothstep(0.0, 1.0, (now - self.fade_t0) as f32 / self.fade_ms as f32);
            for ((s, a), b) in self.shown.iter_mut().zip(&self.from).zip(&self.target) {
                *s = fx::lerp(*a, *b, f);
            }
            animated = true;
        } else {
            self.fade_ms = 0;
            self.shown.copy_from_slice(&self.target);
        }

        let d = self.dim();
        for (c, px) in self.shown.iter().zip(self.px.as_chunks_mut::<3>().0) {
            *px = to_px(*c, d, self.matrix.as_ref());
        }
        animated
    }
}

#[cfg(test)]
mod tests;
