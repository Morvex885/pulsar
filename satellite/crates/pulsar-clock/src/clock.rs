//! 4-digit 7-segment clock, one sysfs LED per segment (names per `Model`).
//!
//! Station Midi: two aw21036/aw21012 chips on i2c 3-0037 / 3-0021,
//! /sys/class/leds/cucumber:white:clock-<seg><digit>.
//!
//! Segment mapping (derived from the live display driven by maind: at 21:17 local time the lit
//! set was a1 b1 d1 e1 g1 | b2 c2 | b3 c3 | a4 b4 c4 plus p3 q3, i.e. "21:17"):
//! clock-<s><n>: n = digit position 1..4 left to right, s = standard 7-segment letter
//!
//! ```text
//!          aaa
//!         f   b
//!          ggg          digits: [1][2] : [3][4]
//!         e   c
//!          ddd
//! ```
//!
//! - p3 + q3: colon (upper/lower dot), the only extra LEDs maind lights for the time.
//! - deg: degree sign; h1 h2, l1 l2, m1 m2, p1 p2 p4 p5, q1 q2 q4 q5: auxiliary dots/icons whose
//!   position is not known (never lit by maind for the time); kept off here.
//! - cucumber:sync:clock-0/1 are the chips' sync pseudo-LEDs (not segments).
//!
//! Midi brightness: like maind, segments are switched 0/255 and dimming uses the chips' global
//! current attribute (/sys/bus/i2c/devices/{3-0037,3-0021}/current, 0..255; maind wrote 5 for
//! its stored brightness 0.0196 = 5/255). The brightness_scale tables are left untouched.
//!
//! Station Lite 2: /sys/class/leds/plum:white:clock<SEG><pos>, SEG = A..G (standard segments),
//! pos = 0..3 left to right, colon = DH + DL; H0..H3 and DEGREE stay dark (maind's /leds/clock
//! frameBuilder config in quasar.cfg), DM only lights in the voice animations
//! (`Model::clock_center`). No global current: maind dims through the per-segment brightness
//! value (0..255), and so do we.
//!
//! Content, first active wins: pairing code (even when disabled) > volume number > voice state
//! (TIMER_RING always, LISTENING / THINKING / SPEAKING with voice indication on) > update install
//! (dark) > one-shot > timer countdown > temporary layer > Text / Face > music levels > time.
//! Disabled: dark.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::path::PathBuf;

use log::{debug, error, info, warn};
use pulsar_core::model::Model;
use pulsar_core::{
    AmbientDimmer, ClockShot, MUSIC_STALE_MS, MusicFrame, SENDSPIN_FRESH_MS, Timers, UiState,
    actors, now_ms, sysfs,
};
use tokio::sync::mpsc;

use crate::clock_fx::{self, Anim, Frame, clamp};
use crate::font::{self, Cell, DIGITS, Glyphs};
use crate::sys::{self, Wall};

const SEGS: usize = 7; // a..g
const TICK_MS: i64 = 250;
/// Every 20 ticks (~5 s) all segments are rewritten even if unchanged (sysfs state may be
/// changed by others); animation frames in between do not count.
const FULL_REWRITE_TICKS: u32 = 20;
const FLASH_MS: i64 = 1500;
const CODE_PAGE_MS: i64 = 2000;
/// Lowest current / brightness that still lights the segments.
const MIN_LEVEL: i32 = 2;
const TEMPORARY_MS: i64 = 5000;
const TEMPORARY_MAX_S: i64 = 3600;
/// Animation frame step (the stock patterns use 20 ms frames).
const FX_FRAME_MS: i64 = 20;
/// Level meter redraw while shown.
const MUSIC_FRAME_MS: i64 = 40;
/// Meters fall with this time constant.
const MUSIC_RELEASE_MS: f32 = 250.0;
/// Each digit's meter spans the recent range of its two bands: the range follows peaks and dips
/// at once and closes in on the current value with this time constant. A mixed song keeps a
/// nearly constant spectral tilt and moves by only a few dB, which on a fixed scale left every
/// song looking alike (measured on a Midi: +-0.15 of 3 segments per digit).
const RANGE_TAU_MS: f32 = 3000.0;
/// Narrowest range, in band units: ~7 dB on the speaker reference (45 dB scale), ~9 dB on
/// Sendspin (60 dB scale); keeps steady passages from blowing noise up to full height.
const MIN_RANGE: f32 = 0.15;
/// A source's ranges start over after this long without it.
const RANGE_KEEP_MS: i64 = 10_000;
/// Decay step across a gap (frames come every ~33 ms).
const RANGE_MAX_STEP_MS: i64 = 100;
/// While the meters are shown, the time takes over for the last `VIZ_TIME_MS` of every cycle.
const VIZ_CYCLE_MS: i64 = 30_000;
const VIZ_TIME_MS: i64 = 3000;

const MUSIC_STYLES: [&str; 2] = ["Off", "Levels"];

/// Names of `set_face` indices: the 17 static faces, then "Blink", "All faces".
pub fn face_names() -> &'static [&'static str] {
    &font::FACE_NAMES
}

/// Clock music styles ("Clock music" select).
pub fn music_styles() -> &'static [&'static str] {
    &MUSIC_STYLES
}

fn shot_anim(s: ClockShot) -> Anim {
    match s {
        ClockShot::Boot => Anim::Boot,
        ClockShot::Pause => Anim::Pause,
        ClockShot::Next => Anim::Next,
        ClockShot::Previous => Anim::Previous,
    }
}

fn glyph_frame(g: Glyphs, colon: bool) -> Frame {
    let mut f = Frame::default();
    for (seg, glyph) in f.seg.iter_mut().zip(g) {
        for (s, v) in seg.iter_mut().enumerate() {
            *v = if glyph >> s & 1 != 0 { 255 } else { 0 };
        }
    }
    f.colon = if colon { 255 } else { 0 };
    f
}

/// Output level (chip current on the Midi, value of a lit segment on the Lite 2) for the user
/// brightness 0..1 and the ambient factor 0.02..1 (1 with auto brightness off): 0 only when the
/// user turned it to 0.
fn level(user_bri: f32, ambient: f32) -> i32 {
    if user_bri <= 0.0 {
        return 0;
    }
    ((255.0 * user_bri * ambient).round() as i32).clamp(MIN_LEVEL, 255)
}

#[derive(Clone, Debug, PartialEq)]
enum Content {
    Text { cells: Vec<Cell>, since: i64 },
    Face { face: i32, since: i64 },
}

impl Content {
    fn since(&self) -> i64 {
        match self {
            Content::Text { since, .. } | Content::Face { since, .. } => *since,
        }
    }

    /// Frame step while it moves; 0 = static.
    fn step(&self, scroll_ms: i64) -> i64 {
        match self {
            Content::Text { cells, .. } if cells.len() > DIGITS => scroll_ms,
            Content::Face { face, .. } if *face == font::FACE_BLINK => font::BLINK_STEP_MS,
            Content::Face { face, .. } if *face == font::FACE_ALL => font::FACE_CYCLE_MS,
            _ => 0,
        }
    }

    fn glyphs(&self, now: i64, scroll_ms: i64) -> (Glyphs, bool) {
        let t = now - self.since();
        match self {
            Content::Text { cells, .. } => font::text_glyphs(cells, t, scroll_ms),
            Content::Face { face, .. } => (font::face_glyphs(*face, t), false),
        }
    }
}

/// Recent band range of one music source, per digit.
#[derive(Clone, Copy, Debug)]
struct Range {
    lo: [f32; DIGITS],
    hi: [f32; DIGITS],
    /// Last frame of this source.
    last_ms: i64,
}

impl Default for Range {
    fn default() -> Self {
        Self {
            lo: [0.0; DIGITS],
            hi: [0.0; DIGITS],
            last_ms: i64::MIN / 2,
        }
    }
}

/// Everything the clock shows, without the output: which layer wins and its frame at a time.
#[derive(Debug)]
struct Layers {
    enabled: bool,
    /// Monotonic ms of a running countdown; 0 = none.
    timer_end: i64,
    /// ms left of a paused countdown; 0 = none.
    timer_paused: i64,
    flash_until: i64,
    flash_value: i32,
    /// Pairing code shown instead of anything else (empty = none).
    code: String,
    code_since: i64,
    /// Text / Select entities.
    base: Option<Content>,
    /// Temporary layer (actions).
    overlay: Option<Content>,
    overlay_until: i64,
    scroll_ms: i64,
    ui: UiState,
    voice_indication: bool,
    /// Voice state / ringing.
    voice: Option<Anim>,
    voice_t0: i64,
    /// One-shot (boot, player).
    shot: Option<Anim>,
    shot_t0: i64,
    updating: bool,
    levels_on: bool,
    media: bool,
    viz: bool,
    /// Per digit, 0..1 within its range.
    mus_in: [f32; DIGITS],
    mus_shown: [f32; DIGITS],
    /// Speaker reference, Sendspin.
    ranges: [Range; 2],
    mus_in_ms: i64,
    sendspin_until: i64,
    mus_drawn_ms: i64,
    viz_t0: i64,
}

impl Layers {
    fn new() -> Self {
        Self {
            enabled: true,
            timer_end: 0,
            timer_paused: 0,
            flash_until: 0,
            flash_value: 0,
            code: String::new(),
            code_since: 0,
            base: None,
            overlay: None,
            overlay_until: 0,
            scroll_ms: 400,
            ui: UiState::Idle,
            voice_indication: true,
            voice: None,
            voice_t0: 0,
            shot: None,
            shot_t0: 0,
            updating: false,
            levels_on: false,
            media: false,
            viz: false,
            mus_in: [0.0; DIGITS],
            mus_shown: [0.0; DIGITS],
            ranges: [Range::default(); 2],
            mus_in_ms: 0,
            sendspin_until: 0,
            mus_drawn_ms: 0,
            viz_t0: 0,
        }
    }

    fn set_timer(&mut self, ms_left: i64, running: bool, now: i64) {
        self.timer_end = if ms_left > 0 && running {
            now + ms_left
        } else {
            0
        };
        self.timer_paused = if ms_left > 0 && !running { ms_left } else { 0 };
    }

    fn flash_number(&mut self, value: i32, now: i64) {
        self.flash_value = value.clamp(-999, 9999);
        self.flash_until = now + FLASH_MS;
    }

    fn show_code(&mut self, digits: String, now: i64) {
        self.code = digits;
        self.code_since = now;
    }

    fn set_text(&mut self, utf8: &str, now: i64) {
        let cells = font::layout_text(utf8.as_bytes());
        if cells.is_empty() {
            // Non-empty text without cells (only ':') still replaces the face, as the
            // controller does.
            if matches!(self.base, Some(Content::Text { .. })) || !utf8.is_empty() {
                self.base = None;
            }
        } else {
            self.base = Some(Content::Text { cells, since: now });
        }
    }

    fn set_face(&mut self, index: i32, now: i64) {
        if !(0..font::FACE_COUNT).contains(&index) {
            if matches!(self.base, Some(Content::Face { .. })) {
                self.base = None;
            }
        } else {
            self.base = Some(Content::Face {
                face: index,
                since: now,
            });
        }
    }

    fn set_scroll_ms(&mut self, ms: i32) {
        self.scroll_ms = i64::from(ms.clamp(100, 2000));
    }

    fn show_temporary_text(&mut self, utf8: &str, seconds: i32, now: i64) {
        let cells = font::layout_text(utf8.as_bytes());
        if cells.is_empty() {
            self.overlay = None;
            return;
        }
        let n = cells.len() as i64;
        let digits = DIGITS as i64;
        self.overlay_until = now
            + if seconds > 0 {
                i64::from(seconds).min(TEMPORARY_MAX_S) * 1000
            } else if n > digits {
                (n + digits) * self.scroll_ms
            } else {
                TEMPORARY_MS
            };
        self.overlay = Some(Content::Text { cells, since: now });
    }

    fn show_temporary_face(&mut self, index: i32, seconds: i32, now: i64) {
        if !(0..font::FACE_COUNT).contains(&index) {
            self.overlay = None;
            return;
        }
        self.overlay_until = now
            + if seconds > 0 {
                i64::from(seconds).min(TEMPORARY_MAX_S) * 1000
            } else {
                TEMPORARY_MS
            };
        self.overlay = Some(Content::Face {
            face: index,
            since: now,
        });
    }

    fn update_voice(&mut self, now: i64) {
        let a = match self.ui {
            UiState::Listening if self.voice_indication => Some(Anim::Listening),
            UiState::Thinking if self.voice_indication => Some(Anim::Thinking),
            UiState::Speaking if self.voice_indication => Some(Anim::Speaking),
            UiState::TimerRing => Some(Anim::Ringing),
            _ => None,
        };
        if a == self.voice {
            return;
        }
        self.voice = a;
        self.voice_t0 = now;
    }

    fn set_ui_state(&mut self, s: UiState, now: i64) {
        self.ui = s;
        self.update_voice(now);
    }

    fn set_voice_indication(&mut self, on: bool, now: i64) {
        self.voice_indication = on;
        self.update_voice(now);
    }

    fn play(&mut self, s: ClockShot, now: i64) {
        self.shot = Some(shot_anim(s));
        self.shot_t0 = now;
    }

    fn music_frame(&mut self, f: &MusicFrame, from_sendspin: bool, now: i64) {
        if from_sendspin {
            self.sendspin_until = now + SENDSPIN_FRESH_MS;
        } else if now < self.sendspin_until {
            return;
        }
        // The sources use different scales, so each keeps its own ranges. Music Assistant
        // restarts the Sendspin stream on every seek / skip (~0.7 s without frames; the speaker
        // reference fills in meanwhile): a range survives such an interlude frozen, and starts
        // over only after RANGE_KEEP_MS without frames of its source.
        let r = &mut self.ranges[usize::from(from_sendspin)];
        let gap = now - r.last_ms;
        let restart = gap >= RANGE_KEEP_MS;
        let k = if restart {
            0.0
        } else {
            (-(gap.min(RANGE_MAX_STEP_MS) as f32) / RANGE_TAU_MS).exp()
        };
        for i in 0..DIGITS {
            let x = (f.bands[2 * i] + f.bands[2 * i + 1]) / 2.0;
            let (lo, hi) = (&mut r.lo[i], &mut r.hi[i]);
            if restart {
                *lo = x;
                *hi = x;
            } else {
                // Fused like the clang-built reference (see clock_fx.rs).
                *hi = if x > *hi { x } else { (*hi - x).mul_add(k, x) };
                *lo = if x < *lo { x } else { (*lo - x).mul_add(k, x) };
            }
            // A range narrower than MIN_RANGE is widened around its centre but kept inside
            // 0..1, so silence (bands at 0) stays dark instead of showing the middle of the
            // meter.
            let span = (*hi - *lo).max(MIN_RANGE);
            let base = clamp((*hi + *lo - span) / 2.0, 0.0, 1.0 - span);
            self.mus_in[i] = clamp((x - base) / span, 0.0, 1.0);
        }
        r.last_ms = now;
        self.mus_in_ms = now;
    }

    /// The frame to show at `now` and the ms to its next change before the tick (0 = none).
    /// `wall` is only asked when the time is shown.
    fn compose(&mut self, now: i64, wall: impl FnOnce() -> Wall) -> (Frame, i64) {
        if self.overlay.is_some() && now >= self.overlay_until {
            self.overlay = None;
        }
        if let Some(s) = self.shot
            && now - self.shot_t0 >= clock_fx::duration_ms(s)
        {
            self.shot = None;
        }
        let eligible = self.levels_on && self.media && now - self.mus_in_ms < MUSIC_STALE_MS;
        if eligible && !self.viz {
            self.viz_t0 = now;
            self.mus_drawn_ms = now;
            self.mus_shown = [0.0; DIGITS];
        }
        self.viz = eligible;
        if !self.code.is_empty() {
            let g = font::code_glyphs(self.code.as_bytes(), now - self.code_since, CODE_PAGE_MS);
            return (glyph_frame(g, false), 0);
        }
        if !self.enabled {
            return (Frame::default(), 0);
        }
        let frame_step = |t0: i64| FX_FRAME_MS - (now - t0) % FX_FRAME_MS;
        if now < self.flash_until {
            let g = font::number_glyphs(self.flash_value);
            (glyph_frame(g, false), self.flash_until - now)
        } else if let Some(v) = self.voice {
            (
                clock_fx::render(v, now - self.voice_t0),
                frame_step(self.voice_t0),
            )
        } else if self.updating {
            // Dark until the new binary takes over.
            (Frame::default(), 0)
        } else if let Some(s) = self.shot {
            let end = self.shot_t0 + clock_fx::duration_ms(s);
            let next = frame_step(self.shot_t0).min(end - now).max(1);
            (clock_fx::render(s, now - self.shot_t0), next)
        } else if self.timer_end != 0 || self.timer_paused != 0 {
            let left_ms = if self.timer_end != 0 {
                self.timer_end - now
            } else {
                self.timer_paused
            };
            let left = ((left_ms + 999) / 1000).max(0);
            let mut g = font::timer_glyphs(left);
            // Blinks at 00:00.
            let blink_off = (now / 500) % 2 != 0;
            let colon = left > 0 || !blink_off;
            if left == 0 && blink_off {
                g = [0; DIGITS];
            }
            (glyph_frame(g, colon), 0)
        } else if let Some((c, is_overlay)) = self
            .overlay
            .as_ref()
            .map(|c| (c, true))
            .or(self.base.as_ref().map(|c| (c, false)))
        {
            let (g, colon) = c.glyphs(now, self.scroll_ms);
            let step = c.step(self.scroll_ms);
            let mut next = if step != 0 {
                step - (now - c.since()) % step
            } else {
                0
            };
            // Also fires when the temporary layer ends, so a single scroll pass does not show
            // the start of a second one until the next tick.
            if is_overlay {
                let left = self.overlay_until - now;
                next = if step != 0 { next.min(left) } else { left }.max(1);
            }
            (glyph_frame(g, colon), next)
        } else if self.viz && (now - self.viz_t0) % VIZ_CYCLE_MS < VIZ_CYCLE_MS - VIZ_TIME_MS {
            let k = (-((now - self.mus_drawn_ms) as f32) / MUSIC_RELEASE_MS).exp();
            for (shown, &input) in self.mus_shown.iter_mut().zip(&self.mus_in) {
                // std::max(in, shown * k): NaN in either keeps `in`, as C++ does.
                let decayed = *shown * k;
                *shown = if input < decayed { decayed } else { input };
            }
            self.mus_drawn_ms = now;
            (clock_fx::levels(&self.mus_shown), MUSIC_FRAME_MS)
        } else {
            let w = wall();
            // Colon on for the first half of every second.
            (
                glyph_frame(font::time_glyphs(w.hour, w.min), w.first_half),
                0,
            )
        }
    }
}

/// The open sysfs attributes and what was last written to them (-1 = unknown).
struct Leds {
    seg: [[Option<File>; SEGS]; DIGITS],
    colon: Vec<File>,
    center: Option<File>,
    current: Vec<File>,
    chip_current: bool,
    seg_last: [[i32; SEGS]; DIGITS],
    colon_last: i32,
    center_last: i32,
    level_last: i32,
    /// Value of a lit segment (the dimmed level when there is no chip current).
    on_value: i32,
}

fn write_int(f: &File, v: i32) -> bool {
    sysfs::write_int_fd(f.as_raw_fd(), v)
}

impl Leds {
    fn seg_name(model: &Model, digit: usize, seg: usize) -> String {
        let letter = char::from_u32(model.clock_seg_base as u32 + seg as u32).unwrap_or('?');
        format!("{letter}{}", model.clock_pos_base + digit as i32)
    }

    fn open_led(model: &Model, name: &str) -> Option<File> {
        let path = format!("{}{name}/brightness", model.clock_dir);
        OpenOptions::new().write(true).open(path).ok()
    }

    /// The segments, colon and centre LEDs of `model`; aux LEDs are switched off once. None
    /// (logged) when no segment exists.
    fn open(model: &Model) -> Option<Leds> {
        let mut found = 0;
        let seg = std::array::from_fn(|d| {
            std::array::from_fn(|s| {
                let f = Self::open_led(model, &Self::seg_name(model, d, s));
                if f.is_some() {
                    found += 1;
                }
                f
            })
        });
        let total = DIGITS * SEGS;
        if found == 0 {
            // The whole panel is absent: one line, not one per LED.
            if model.has_clock {
                error!("clock: no clock segments under {}", model.clock_dir);
            } else {
                info!("clock: no clock segments under {}", model.clock_dir);
            }
            return None;
        }
        if found < total {
            warn!("clock: {} of {total} segments missing", total - found);
        }
        let colon: Vec<File> = model
            .clock_colon
            .iter()
            .filter_map(|name| Self::open_led(model, name))
            .collect();
        for aux in model.clock_aux {
            if let Some(f) = Self::open_led(model, aux) {
                write_int(&f, 0);
            }
        }
        let mut center = None;
        if !model.clock_center.is_empty() {
            center = Self::open_led(model, model.clock_center);
            if center.is_none() {
                warn!("clock: missing centre LED {}", model.clock_center);
            }
        }
        let mut current = Vec::new();
        if model.clock_chip_current {
            // Global current of the clock driver chips, found through the LED device links of
            // the leftmost and rightmost digit (one chip each on the Midi).
            let mut opened: Vec<PathBuf> = Vec::new();
            for name in [
                Self::seg_name(model, 0, 0),
                Self::seg_name(model, DIGITS - 1, 0),
            ] {
                let dev = format!("{}{name}/device/current", model.clock_dir);
                let Ok(real) = std::fs::canonicalize(dev) else {
                    continue;
                };
                if opened.contains(&real) {
                    continue;
                }
                if let Ok(f) = OpenOptions::new().write(true).open(&real) {
                    current.push(f);
                    opened.push(real);
                }
            }
        }
        info!(
            "clock: {found} segments, {} colon LEDs, {} dimming",
            colon.len(),
            if model.clock_chip_current {
                "chip current"
            } else {
                "segment brightness"
            }
        );
        Some(Leds {
            seg,
            colon,
            center,
            current,
            chip_current: model.clock_chip_current,
            seg_last: [[-1; SEGS]; DIGITS],
            colon_last: -1,
            center_last: -1,
            level_last: -1,
            on_value: 255,
        })
    }

    fn forget_written(&mut self) {
        self.seg_last = [[-1; SEGS]; DIGITS];
        self.colon_last = -1;
        self.center_last = -1;
    }

    fn scaled(&self, v: u8) -> i32 {
        (i32::from(v) * self.on_value + 127) / 255
    }

    /// Writes what changed since the last write.
    fn draw(&mut self, f: &Frame) {
        for d in 0..DIGITS {
            for s in 0..SEGS {
                let x = self.scaled(f.seg[d][s]);
                if self.seg_last[d][s] == x {
                    continue;
                }
                if let Some(file) = &self.seg[d][s]
                    && write_int(file, x)
                {
                    self.seg_last[d][s] = x;
                }
            }
        }
        let x = self.scaled(f.colon);
        if self.colon_last != x {
            for file in &self.colon {
                write_int(file, x);
            }
            self.colon_last = x;
        }
        let x = self.scaled(f.center);
        if self.center_last != x
            && let Some(file) = &self.center
            && write_int(file, x)
        {
            self.center_last = x;
        }
    }

    /// Applies an output level; true if it changed.
    fn set_level(&mut self, lvl: i32) -> bool {
        if lvl == self.level_last {
            return false;
        }
        self.level_last = lvl;
        if self.chip_current {
            for file in &self.current {
                write_int(file, lvl);
            }
        } else {
            // Lit segments are rewritten with the new value on the next refresh.
            self.on_value = lvl;
            self.forget_written();
        }
        true
    }
}

enum Cmd {
    SetEnabled(bool),
    SetBrightness(f32),
    SetAmbientLux(f32),
    SetAutoBrightness(bool),
    SetTimer { ms_left: i64, running: bool },
    FlashNumber(i32),
    ShowCode(String),
    ClearCode,
    SetText(String),
    SetFace(i32),
    SetScrollMs(i32),
    ShowTemporaryText(String, i32),
    ShowTemporaryFace(i32, i32),
    SetUiState(UiState),
    SetVoiceIndication(bool),
    Play(ClockShot),
    SetUpdating(bool),
    SetMusicStyle(String),
    SetMediaPlaying(bool),
    MusicFrame(MusicFrame, bool),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Timer {
    Tick,
    /// One-shot frame timer of moving content, re-armed by every refresh.
    Anim,
}

struct Clock {
    layers: Layers,
    leds: Leds,
    user_bri: f32,
    /// < 0: unknown, no auto-dimming (only logged).
    lux: f32,
    dimmer: AmbientDimmer,
    /// false: the level is the user brightness alone; readings still feed `dimmer`, so turning
    /// it back on applies the current light at once.
    auto_bri: bool,
    refreshes: u32,
    timers: Timers<Timer>,
}

impl Clock {
    fn apply_level(&mut self) {
        let ambient = if self.auto_bri {
            self.dimmer.factor()
        } else {
            1.0
        };
        let lvl = level(self.user_bri, ambient);
        if self.leds.set_level(lvl) {
            debug!(
                "clock: level={lvl} (user {:.2}, lux {:.1})",
                self.user_bri, self.lux
            );
        }
    }

    fn refresh(&mut self) {
        let (frame, next) = self.layers.compose(now_ms(), sys::wall);
        self.leds.draw(&frame);
        self.timers.cancel(Timer::Anim);
        if next > 0 {
            self.timers.set(Timer::Anim, next as u64);
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        let now = now_ms();
        let l = &mut self.layers;
        match cmd {
            Cmd::SetEnabled(on) => l.enabled = on,
            Cmd::SetBrightness(b) => {
                // NaN counts as full brightness, as std::max(0, std::min(1, b)) did.
                self.user_bri = if b.is_nan() { 1.0 } else { b.clamp(0.0, 1.0) };
                self.apply_level();
            }
            Cmd::SetAmbientLux(lux) => {
                self.lux = lux.max(0.0);
                self.dimmer.update(self.lux);
                self.apply_level();
            }
            Cmd::SetAutoBrightness(on) => {
                self.auto_bri = on;
                self.apply_level();
            }
            Cmd::SetTimer { ms_left, running } => l.set_timer(ms_left, running, now),
            Cmd::FlashNumber(v) => l.flash_number(v, now),
            Cmd::ShowCode(digits) => l.show_code(digits, now),
            Cmd::ClearCode => l.code.clear(),
            Cmd::SetText(s) => l.set_text(&s, now),
            Cmd::SetFace(i) => l.set_face(i, now),
            Cmd::SetScrollMs(ms) => l.set_scroll_ms(ms),
            Cmd::ShowTemporaryText(s, secs) => l.show_temporary_text(&s, secs, now),
            Cmd::ShowTemporaryFace(i, secs) => l.show_temporary_face(i, secs, now),
            Cmd::SetUiState(s) => l.set_ui_state(s, now),
            Cmd::SetVoiceIndication(on) => l.set_voice_indication(on, now),
            Cmd::Play(s) => l.play(s, now),
            Cmd::SetUpdating(on) => l.updating = on,
            Cmd::SetMusicStyle(style) => l.levels_on = style == "Levels",
            Cmd::SetMediaPlaying(on) => l.media = on,
            Cmd::MusicFrame(f, from_sendspin) => {
                // Frames only feed the meters; the frame timer draws them.
                l.music_frame(&f, from_sendspin, now);
                return;
            }
        }
        self.refresh();
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            tokio::select! {
                biased;
                t = self.timers.next() => {
                    if t == Timer::Tick {
                        self.refreshes = self.refreshes.wrapping_add(1);
                        if self.refreshes.is_multiple_of(FULL_REWRITE_TICKS) {
                            self.leds.forget_written();
                        }
                    }
                    self.refresh();
                }
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd),
                    None => break,
                },
            }
        }
    }
}

/// Whether any clock segment of `model` exists in sysfs (decides the clock entities of a model
/// whose panel may be missing, `Model::has_clock` false).
pub fn panel_present(model: &Model) -> bool {
    (0..DIGITS).any(|d| {
        (0..SEGS).any(|s| {
            let led = format!(
                "{}{}/brightness",
                model.clock_dir,
                Leds::seg_name(model, d, s)
            );
            std::path::Path::new(&led).exists()
        })
    })
}

/// Clock control; every method queues a command for the clock actor and returns at once. The
/// handle of a clock that could not be opened does nothing.
#[derive(Clone)]
pub struct ClockHandle {
    tx: Option<mpsc::UnboundedSender<Cmd>>,
}

/// Opens the clock LEDs of `model` (sysfs `<clock_dir><segment><position>`) and starts the
/// clock actor, which stops when every handle is dropped. Without any segment: logged ("clock
/// unavailable"), the handle is a no-op.
pub fn spawn(model: &'static Model) -> ClockHandle {
    let Some(leds) = Leds::open(model) else {
        if model.has_clock {
            warn!("clock unavailable");
        }
        return ClockHandle { tx: None };
    };
    let mut clock = Clock {
        layers: Layers::new(),
        leds,
        user_bri: 1.0,
        lux: -1.0,
        dimmer: AmbientDimmer::new(),
        auto_bri: true,
        refreshes: 0,
        timers: Timers::new(),
    };
    let tick = TICK_MS as u64;
    clock.timers.set_repeating(Timer::Tick, tick, tick);
    clock.apply_level();
    clock.refresh();
    let (tx, rx) = mpsc::unbounded_channel();
    actors::spawn_actor("clock", clock.run(rx));
    ClockHandle { tx: Some(tx) }
}

impl ClockHandle {
    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cmd);
        }
    }

    pub fn set_enabled(&self, on: bool) {
        self.send(Cmd::SetEnabled(on));
    }

    /// 0..1 user scale; with auto brightness on, the ceiling of the ambient auto-dimming.
    pub fn set_brightness(&self, b: f32) {
        self.send(Cmd::SetBrightness(b));
    }

    pub fn set_ambient_lux(&self, lux: f32) {
        self.send(Cmd::SetAmbientLux(lux));
    }

    /// On: the level follows the light sensor (the 0..1 scale is the ceiling). Off: the level
    /// is the 0..1 scale alone, whatever the light.
    pub fn set_auto_brightness(&self, on: bool) {
        self.send(Cmd::SetAutoBrightness(on));
    }

    /// Shows a countdown mm:ss (or h:mm) instead of the time while `ms_left > 0`; <= 0 clears.
    /// `running`: counts down on its own from `ms_left`; otherwise `ms_left` stays shown
    /// (paused timer).
    pub fn set_timer(&self, ms_left: i64, running: bool) {
        self.send(Cmd::SetTimer { ms_left, running });
    }

    /// Shows a number (volume 0..100) for 1.5 s.
    pub fn flash_number(&self, value: i32) {
        self.send(Cmd::FlashNumber(value));
    }

    /// Shows a pairing code (digits) until `clear_code`, over everything else, even when the
    /// clock is disabled. Codes longer than 4 digits page through 3-digit groups.
    pub fn show_code(&self, digits: String) {
        self.send(Cmd::ShowCode(digits));
    }

    pub fn clear_code(&self) {
        self.send(Cmd::ClearCode);
    }

    /// Custom text (Text entity), UTF-8: one cell per character, ':' lights the colon after the
    /// preceding character instead of taking a cell; up to 4 cells are shown static and
    /// centred, longer text scrolls right-to-left in a loop. `set_text` and `set_face` replace
    /// each other; `set_text("")` / `set_face(-1)` clear only their own kind.
    pub fn set_text(&self, utf8: String) {
        self.send(Cmd::SetText(utf8));
    }

    /// Index into `face_names()`; -1 = none.
    pub fn set_face(&self, index: i32) {
        self.send(Cmd::SetFace(index));
    }

    /// Scroll step, clamped to 100..2000 ms.
    pub fn set_scroll_ms(&self, ms: i32) {
        self.send(Cmd::SetScrollMs(ms));
    }

    /// Temporary layer above the text / face. `seconds <= 0`: one pass of a scrolling text, 5 s
    /// otherwise; capped at 3600 s. Replaces the previous layer; "" removes it.
    pub fn show_temporary_text(&self, utf8: String, seconds: i32) {
        self.send(Cmd::ShowTemporaryText(utf8, seconds));
    }

    /// As `show_temporary_text` with a face; `seconds <= 0`: 5 s; -1 removes the layer.
    pub fn show_temporary_face(&self, index: i32, seconds: i32) {
        self.send(Cmd::ShowTemporaryFace(index, seconds));
    }

    pub fn set_ui_state(&self, s: UiState) {
        self.send(Cmd::SetUiState(s));
    }

    pub fn set_voice_indication(&self, on: bool) {
        self.send(Cmd::SetVoiceIndication(on));
    }

    /// Restarts / replaces a running one-shot.
    pub fn play(&self, s: ClockShot) {
        self.send(Cmd::Play(s));
    }

    /// Dark while an update installs.
    pub fn set_updating(&self, on: bool) {
        self.send(Cmd::SetUpdating(on));
    }

    /// One of `music_styles()`.
    pub fn set_music_style(&self, style: String) {
        self.send(Cmd::SetMusicStyle(style));
    }

    pub fn set_media_playing(&self, playing: bool) {
        self.send(Cmd::SetMediaPlaying(playing));
    }

    /// Music features, ~30 per second while playing; Sendspin frames take precedence over the
    /// speaker reference while they keep coming. Shown only while media plays and the style is
    /// not "Off".
    pub fn music_frame(&self, f: MusicFrame, from_sendspin: bool) {
        self.send(Cmd::MusicFrame(f, from_sendspin));
    }
}

#[cfg(test)]
mod tests;
