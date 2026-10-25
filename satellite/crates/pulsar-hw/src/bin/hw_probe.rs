//! Hardware probe, run with the daemon stopped.
//! usage: hw_probe leds [state_ms=2000]   cycle LED states, effects, music styles (synthetic
//!                                        music at 30 fps), dump sysfs; ring dark after exit
//!        hw_probe input [seconds=15]      print buttons and the mute switch
//!        hw_probe clock [state_ms=2000]   time, number, timer, levels, animations, text, face,
//!                                        code, music levels; sysfs read back and checked
//!                                        (state_ms >= 2000: the code pages every 2 s and the
//!                                        1.5 s number is checked at 1/2 of its slot)
//!        hw_probe light [seconds=10]      print the light sensor every second
//!        hw_probe all [input_seconds=15] [state_ms=2000]
//!                                        the LED and clock steps, then input and lux (the
//!                                        whole C++ hw_probe)
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pulsar_clock::ClockHandle;
use pulsar_core::model::{LedStrip, Model};
use pulsar_core::{AmbientDimmer, ClockShot, MusicFrame, UiState, actors};
use pulsar_hw::{Button, InputEvent, InputHandle, LedsHandle, Press, Rgb};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::LocalSet;

const USAGE: &str = "usage: hw_probe leds [state_ms=2000] | input [seconds=15] | \
                     clock [state_ms=2000] | light [seconds=10] | \
                     all [input_seconds=15] [state_ms=2000]";

/// Output byte of a full-brightness channel without ambient dimming: 255 x the 80 % cap.
const FULL_FB: u8 = 204;

/// stdout without panicking on a closed pipe.
fn say(line: &str) {
    let _ = writeln!(std::io::stdout(), "{line}");
}

fn main() -> ExitCode {
    pulsar_core::init_logger();
    pulsar_core::add_own_target(module_path!());
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize, default: u64| args.get(i).map_or(Some(default), |s| s.parse::<u64>().ok());
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let (a, b) = match cmd {
        "leds" | "clock" => (arg(1, 2000), Some(0)),
        "input" => (arg(1, 15), Some(0)),
        "light" => (arg(1, 10), Some(0)),
        "all" => (arg(1, 15), arg(2, 2000)),
        _ => (None, None),
    };
    let (Some(a), Some(b)) = (a, b) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let Some(model) = pulsar_core::model::detect_model() else {
        return ExitCode::FAILURE;
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(4)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let local = LocalSet::new();
    let ok = local.block_on(&rt, async {
        let ok = match cmd {
            "leds" => leds(model, a).await,
            "input" => input(model, a).await,
            "clock" => clock(model, a).await,
            "light" => light(a).await,
            _ => all(model, a, b).await,
        };
        actors::join_all(Duration::from_secs(3)).await && ok
    });
    {
        // Tasks still running are dropped inside the runtime, which their resources belong to.
        let _rt = rt.enter();
        drop(local);
    }
    // A plain drop would wait for spawn_blocking work without limit.
    rt.shutdown_timeout(Duration::from_secs(1));
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// -------------------------------------------------------------------------------------------------
// leds

fn led_value(model: &Model, s: &LedStrip, color: &str, i: i32) -> i32 {
    let path = format!(
        "/sys/class/leds/{}:{color}:{}{}/brightness",
        model.led_prefix,
        s.sysfs_kind,
        s.first_index + i
    );
    pulsar_core::sysfs::read_file(&path)
        .and_then(|v| pulsar_core::sysfs::trim(&v).parse().ok())
        .unwrap_or(0)
}

fn rgb_values(model: &Model, s: &LedStrip, i: i32) -> [i32; 3] {
    ["red", "green", "blue"].map(|c| led_value(model, s, c, i))
}

/// Every LED of a short ring, every third of the Midi's; the first logo LED.
fn ring_dump(model: &Model) -> String {
    let hex = |v: [i32; 3]| format!("{:02x}{:02x}{:02x}", v[0], v[1], v[2]);
    let step = if model.ring.count > 6 { 3 } else { 1 };
    let mut s = String::new();
    for i in (0..model.ring.count).step_by(step) {
        s += &format!("{i}:{} ", hex(rgb_values(model, &model.ring, i)));
    }
    if model.logo.count > 0 {
        s += &format!("logo0:{}", hex(rgb_values(model, &model.logo, 0)));
    }
    s
}

/// sysfs brightness of a framebuffer byte per channel. The Lite 2's led_ring_fb scales each
/// channel on its way to the LEDs (AGENTS.md, "Lite 2 LEDs") and the brightness tops out at 63
/// (measured with the daemon stopped: fb 26/100/150/204/255 -> red 9/37/55/63/63, green
/// 6/24/36/49/62, blue 4/19/29/39/49); the Midi's reads back as written.
fn sysfs_expected(model: &Model, fb: [u8; 3]) -> [i32; 3] {
    if model.codename == "plum" {
        let k = [0.36, 0.23, 0.2];
        [0, 1, 2].map(|c| ((f32::from(fb[c]) * k[c]).round() as i32).min(63))
    } else {
        fb.map(i32::from)
    }
}

/// Whether the daemon would drive `s` through its framebuffer: the same test as pulsar-hw
/// (trigger active on the first red LED, fb named as expected).
fn fb_mode(model: &Model, s: &LedStrip) -> bool {
    let trigger = pulsar_core::sysfs::read_file(&format!(
        "/sys/class/leds/{}:red:{}{}/trigger",
        model.led_prefix, s.sysfs_kind, s.first_index
    ))
    .unwrap_or_default();
    let dev = s.fb_dev.rsplit('/').next().unwrap_or(s.fb_dev);
    let name = pulsar_core::sysfs::read_file(&format!("/sys/class/graphics/{dev}/name"))
        .unwrap_or_default();
    trigger.contains(&format!("[{}]", s.trigger)) && pulsar_core::sysfs::trim(&name) == s.fb_name
}

/// The strip's framebuffer as RGB per LED; None when it cannot be read.
fn strip_fb(s: &LedStrip) -> Option<Vec<[u8; 3]>> {
    let n = usize::try_from(s.count).ok()?;
    let mut bgr = vec![0u8; n * 3];
    std::fs::File::open(s.fb_dev)
        .ok()?
        .read_exact_at(&mut bgr, 0)
        .ok()?;
    Some(
        bgr.as_chunks::<3>()
            .0
            .iter()
            .map(|p| [p[2], p[1], p[0]])
            .collect(),
    )
}

/// Checks that every pixel of the strip's framebuffer is `fb` (in framebuffer mode) and that
/// every LED's sysfs brightness matches it as the model reads it back (within ±3).
fn check_strip(model: &Model, s: &LedStrip, what: &str, fb: [u8; 3]) -> bool {
    let want = sysfs_expected(model, fb);
    let mut ok = true;
    let mut got = String::new();
    for i in 0..s.count {
        let v = rgb_values(model, s, i);
        ok &= v.iter().zip(want).all(|(g, w)| (g - w).abs() <= 3);
        got += &format!(" {},{},{}", v[0], v[1], v[2]);
    }
    // pulsar-hw uses the framebuffers only if the ring's and the logo's both qualify.
    let fb_output =
        fb_mode(model, &model.ring) && (model.logo.count == 0 || fb_mode(model, &model.logo));
    let fb_state = if !fb_output {
        "fb not checked (sysfs output)".to_owned()
    } else {
        match strip_fb(s) {
            Some(px) if px.iter().all(|p| *p == fb) => "fb ok".to_owned(),
            Some(px) => {
                ok = false;
                format!("fb differs: {px:?}")
            }
            None => {
                ok = false;
                "fb not readable".to_owned()
            }
        }
    };
    say(&format!(
        "   check {what} ({}): fb {},{},{} {fb_state}; sysfs want {},{},{} got{got}: {}",
        s.sysfs_kind,
        fb[0],
        fb[1],
        fb[2],
        want[0],
        want[1],
        want[2],
        if ok { "PASS" } else { "FAIL" }
    ));
    ok
}

/// Raw framebuffer write rate (ring, 72 bytes).
fn fb_rate() {
    let Ok(fb) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fb0")
    else {
        return;
    };
    let zero = [0u8; 72];
    let t0 = Instant::now();
    for _ in 0..100 {
        let _ = fb.write_at(&zero, 0);
    }
    say(&format!(
        "fb0: 100 frame writes in {} ms",
        t0.elapsed().as_millis()
    ));
}

/// Clock check at the 3/4 dump; false = failed (it prints why).
type ClockCheck = Box<dyn Fn(&Model) -> bool>;

struct Step {
    name: String,
    run: Box<dyn Fn()>,
    /// Ring colour checked at the 3/4 dump.
    check: Option<[u8; 3]>,
    clock_check: Option<ClockCheck>,
    /// Clock check at the 1/2 dump (content shorter than the slot).
    half_check: Option<ClockCheck>,
}

fn step(name: impl Into<String>, run: impl Fn() + 'static) -> Step {
    Step {
        name: name.into(),
        run: Box::new(run),
        check: None,
        clock_check: None,
        half_check: None,
    }
}

/// Synthetic music features at 30 frames/s to the LEDs and / or the clock while `on`, until
/// `done`.
fn feed_music(
    leds: Option<LedsHandle>,
    clock: Option<ClockHandle>,
    on: Rc<Cell<bool>>,
    done: Rc<Cell<bool>>,
) {
    actors::spawn_actor("music feed", async move {
        let t0 = Instant::now();
        let mut last_beat: Option<Instant> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(33));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !done.get() {
            tick.tick().await;
            if !on.get() {
                continue;
            }
            let t = t0.elapsed().as_secs_f64() * 1000.0;
            let mut f = MusicFrame::default();
            for (b, v) in f.bands.iter_mut().enumerate() {
                *v = (0.5 + 0.5 * (std::f64::consts::TAU * (t / 800.0 + b as f64 / 8.0)).sin())
                    as f32;
            }
            f.loudness = (0.5 + 0.5 * (std::f64::consts::TAU * t / 1000.0).sin()) as f32;
            if last_beat.is_none_or(|b| b.elapsed() >= Duration::from_millis(500)) {
                f.beat = true;
                last_beat = Some(Instant::now());
            }
            if let Some(l) = &leds {
                l.music_frame(f, false);
            }
            if let Some(c) = &clock {
                c.music_frame(f, false);
            }
        }
    });
}

fn led_steps(leds: &LedsHandle, music: &Rc<Cell<bool>>) -> Vec<Step> {
    let l = leds.clone();
    let st = move |s: UiState| {
        let l = l.clone();
        move || l.set_ui_state(s)
    };
    let mut steps = vec![
        step("LISTENING", st(UiState::Listening)),
        step("THINKING", st(UiState::Thinking)),
        step("SPEAKING", st(UiState::Speaking)),
        step("ERROR", st(UiState::Error)),
        // IDLE during the error is ignored: it runs its 3.54 s to the end, then the ring is off.
        step(
            "IDLE requested during ERROR (off once it ends)",
            st(UiState::Idle),
        ),
        step("TIMER_RING", st(UiState::TimerRing)),
    ];
    let l = leds.clone();
    steps.push(step("IDLE, mic muted", move || {
        l.set_ui_state(UiState::Idle);
        l.set_muted(true);
    }));
    let l = leds.clone();
    steps.push(step("mic unmuted", move || l.set_muted(false)));
    for &e in pulsar_hw::effect_names() {
        const WHITE: [&str; 7] = [
            "Rainbow",
            "Color loop",
            "Fire",
            "Aurora",
            "Lava lamp",
            "Candle",
            "Night light",
        ];
        let col = if WHITE.contains(&e) {
            Rgb {
                r: 255,
                g: 255,
                b: 255,
            }
        } else {
            Rgb {
                r: 255,
                g: 0,
                b: 255,
            }
        };
        let l = leds.clone();
        steps.push(step(format!("IDLE user {e}"), move || {
            l.set_ui_state(UiState::Idle);
            l.set_user_light(true, col, 0.6, e.to_owned(), 0);
        }));
    }
    let l = leds.clone();
    let mut red = step("solid red, full brightness", move || {
        l.set_user_light(true, Rgb { r: 255, g: 0, b: 0 }, 1.0, String::new(), 0);
    });
    red.check = Some([FULL_FB, 0, 0]);
    steps.push(red);
    let l = leds.clone();
    steps.push(step("led_show Comet green 3 s", move || {
        l.show_temporary("Comet".into(), Rgb { r: 0, g: 255, b: 0 }, 3);
    }));
    let l = leds.clone();
    steps.push(step("show_volume 0.2 (user light off)", move || {
        l.clear_temporary();
        l.set_user_light(false, Rgb { r: 0, g: 0, b: 0 }, 0.0, String::new(), 0);
        l.show_volume(0.2);
    }));
    for v in [0.6f32, 1.0] {
        let l = leds.clone();
        steps.push(step(format!("show_volume {v:.1}"), move || {
            l.show_volume(v)
        }));
    }
    for &style in &pulsar_hw::music_styles()[2..] {
        let l = leds.clone();
        let m = Rc::clone(music);
        steps.push(step(format!("music {style}"), move || {
            m.set(true);
            l.set_media_playing(true);
            l.set_music_style(style.to_owned());
        }));
    }
    if let Some(c) = pulsar_hw::case_colors().into_iter().next() {
        let l = leds.clone();
        steps.push(step(format!("solid white, case colour {c}"), move || {
            l.set_media_playing(false);
            l.set_user_light(
                true,
                Rgb {
                    r: 255,
                    g: 255,
                    b: 255,
                },
                0.6,
                String::new(),
                0,
            );
            l.set_case_color(&c);
        }));
    }
    let l = leds.clone();
    let m = Rc::clone(music);
    steps.push(step("music off, light off, no case colour", move || {
        m.set(false);
        l.set_media_playing(false);
        l.set_user_light(false, Rgb { r: 0, g: 0, b: 0 }, 0.0, String::new(), 0);
        l.set_case_color("");
    }));
    // Shown above the lamp and music for 15 s: last of the LED steps.
    let l = leds.clone();
    steps.push(step("timer started (15 s)", move || l.show_timer_started()));
    // The 600 ms mute sweep shows over the countdown, which then goes on (green at the dumps).
    let l = leds.clone();
    steps.push(step("mic muted during the countdown", move || {
        l.set_muted(true)
    }));
    let l = leds.clone();
    steps.push(step("mic unmuted during the countdown", move || {
        l.set_muted(false);
    }));
    steps
}

/// Which sysfs dumps the steps print.
#[derive(Clone, Copy)]
struct Dumps {
    ring: bool,
    clock: bool,
}

/// Runs step i at t0 + i·state_ms, dumping at 1/2 and 3/4 of its slot (one time base, like the
/// C++ probe's timers) and checking at 3/4; false if a check failed.
async fn run_steps(model: &Model, steps: Vec<Step>, state_ms: u64, dumps: Dumps) -> bool {
    let t0 = tokio::time::Instant::now();
    let at = |ms: u64| tokio::time::sleep_until(t0 + Duration::from_millis(ms));
    let mut ok = true;
    let mut end = 0;
    for (i, s) in (0u64..).zip(steps) {
        let start = i * state_ms;
        end = start + state_ms;
        at(start).await;
        say(&format!("== {}", s.name));
        (s.run)();
        at(start + state_ms / 2).await;
        if dumps.ring {
            say(&format!("   ring {}", ring_dump(model)));
        }
        if dumps.clock {
            say(&format!("   clock {}", clock_dump(model)));
            say(&format!("   values {}", clock_values(model)));
        }
        if let Some(check) = &s.half_check {
            ok &= check(model);
        }
        at(start + state_ms * 3 / 4).await;
        if dumps.ring {
            say(&format!("   ring {}", ring_dump(model)));
        }
        if dumps.clock {
            say(&format!("   values {}", clock_values(model)));
        }
        if let Some(fb) = s.check {
            ok &= check_strip(model, &model.ring, &s.name, fb);
        }
        if let Some(check) = &s.clock_check {
            ok &= check(model);
        }
    }
    at(end).await;
    ok
}

/// After the LED steps: every handle gone, the actor turns the ring and logo off.
async fn leds_off_after_exit(model: &Model) -> bool {
    let stopped = actors::join_all(Duration::from_secs(3)).await;
    say(&format!("== exit (actors stopped: {stopped})"));
    say(&format!("   ring {}", ring_dump(model)));
    let mut off = check_strip(model, &model.ring, "off after exit", [0; 3]);
    if model.logo.count > 0 {
        off &= check_strip(model, &model.logo, "off after exit", [0; 3]);
    }
    off && stopped
}

async fn leds(model: &'static Model, state_ms: u64) -> bool {
    fb_rate();
    let leds = pulsar_hw::spawn_leds(model);
    let music = Rc::new(Cell::new(false));
    let done = Rc::new(Cell::new(false));
    feed_music(
        Some(leds.clone()),
        None,
        Rc::clone(&music),
        Rc::clone(&done),
    );
    // Initial state, no indication.
    leds.set_muted(false);
    let steps = led_steps(&leds, &music);
    let dumps = Dumps {
        ring: true,
        clock: false,
    };
    let ok = run_steps(model, steps, state_ms, dumps).await;
    done.set(true);
    drop(leds);
    leds_off_after_exit(model).await && ok
}

// -------------------------------------------------------------------------------------------------
// input

async fn input(model: &'static Model, seconds: u64) -> bool {
    let (input, rx) = match pulsar_hw::spawn_input(model) {
        Ok(v) => v,
        Err(e) => {
            say(&format!("open: input=0 ({e})"));
            return false;
        }
    };
    say(&format!(
        "open: input=1 hw_muted={}",
        u8::from(input.hw_muted())
    ));
    say(&format!(
        "== monitoring input for {seconds} s (press buttons, toggle mute)"
    ));
    monitor(Some(rx), None, seconds).await;
    say(&format!("final hw_muted={}", u8::from(input.hw_muted())));
    true
}

/// Prints buttons, mute and lux until `seconds` are up; the number of lux readings.
async fn monitor(
    mut input: Option<UnboundedReceiver<InputEvent>>,
    mut light: Option<UnboundedReceiver<f32>>,
    seconds: u64,
) -> u32 {
    let mut readings = 0;
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    while input.is_some() || light.is_some() {
        tokio::select! {
            () = &mut deadline => break,
            ev = async { input.as_mut()?.recv().await }, if input.is_some() => match ev {
                Some(InputEvent::Button(b, p)) => {
                    let b = match b {
                        Button::Action => "ACTION",
                        Button::VolumeUp => "VOLUME_UP",
                        Button::VolumeDown => "VOLUME_DOWN",
                    };
                    let p = if p == Press::Long { "LONG" } else { "SHORT" };
                    say(&format!("BUTTON {b} {p}"));
                }
                Some(InputEvent::Mute(m)) => say(&format!("MUTE {}", if m { "on" } else { "off" })),
                None => input = None,
            },
            lux = async { light.as_mut()?.recv().await }, if light.is_some() => match lux {
                Some(lux) => {
                    readings += 1;
                    say(&format!("lux {lux:.2}"));
                }
                None => light = None,
            },
        }
    }
    readings
}

// -------------------------------------------------------------------------------------------------
// clock

/// The Midi's clock driver chips (C++ hw_probe: dumped and restored).
const MIDI_CURRENT: [&str; 2] = [
    "/sys/bus/i2c/devices/3-0037/current",
    "/sys/bus/i2c/devices/3-0021/current",
];

/// Segment masks the C++ probe reads back: digits 0..9, '-'.
const FONT: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
const MINUS: u8 = 0x40;
/// "Smiley", the first face.
const SMILEY: [u8; 4] = [0x40, 0x18, 0x0C, 0x40];

fn read_trimmed(path: &str) -> Option<String> {
    pulsar_core::sysfs::read_file(path).map(|s| pulsar_core::sysfs::trim(&s).to_owned())
}

fn seg_name(model: &Model, digit: usize, seg: usize) -> String {
    let letter = char::from_u32(model.clock_seg_base as u32 + seg as u32).unwrap_or('?');
    format!("{letter}{}", model.clock_pos_base + digit as i32)
}

/// Brightness of a clock LED, clamped to 0..255; 0 when unreadable.
fn clock_led(model: &Model, name: &str) -> i32 {
    read_trimmed(&format!("{}{name}/brightness", model.clock_dir))
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0)
        .clamp(0, 255)
}

/// Lit segments per digit (bit i = segment 'a' + i) and the colon.
fn clock_masks(model: &Model) -> ([u8; 4], bool) {
    let masks = std::array::from_fn(|d| {
        (0..7)
            .filter(|&s| clock_led(model, &seg_name(model, d, s)) > 0)
            .fold(0u8, |m, s| m | 1 << s)
    });
    let colon = model
        .clock_colon
        .first()
        .is_some_and(|c| clock_led(model, c) > 0);
    (masks, colon)
}

fn masks_str(masks: [u8; 4], colon: bool) -> String {
    let mut s = String::new();
    for (d, &m) in masks.iter().enumerate() {
        s.push(match m {
            0 => ' ',
            MINUS => '-',
            _ => FONT
                .iter()
                .position(|&f| f == m)
                .map_or('?', |i| char::from(b'0' + i as u8)),
        });
        if d == 1 {
            s.push(if colon { ':' } else { ' ' });
        }
    }
    s
}

/// "[hh:mm]" as read back, plus the chip currents on the Midi.
fn clock_dump(model: &Model) -> String {
    let (masks, colon) = clock_masks(model);
    let s = format!("[{}]", masks_str(masks, colon));
    if !model.clock_chip_current {
        return s;
    }
    let cur = |p: &str| read_trimmed(p).unwrap_or_else(|| "?".into());
    format!(
        "{s} current={}/{}",
        cur(MIDI_CURRENT[0]),
        cur(MIDI_CURRENT[1])
    )
}

/// Brightness of every segment (a..g per digit, 2 hex digits each) and the centre LED.
fn clock_values(model: &Model) -> String {
    let mut s = String::new();
    for d in 0..4 {
        for seg in 0..7 {
            s += &format!("{:02x}", clock_led(model, &seg_name(model, d, seg)));
        }
        s.push(' ');
    }
    if !model.clock_center.is_empty() {
        s += &format!("c={:02x}", clock_led(model, model.clock_center));
    }
    s
}

/// The probe sets no TZ, so the clock shows UTC (the stations have no /etc/localtime).
fn utc_hhmm(secs: u64) -> [u8; 4] {
    let (h, m) = ((secs / 3600 % 24) as usize, (secs / 60 % 60) as usize);
    [FONT[h / 10], FONT[h % 10], FONT[m / 10], FONT[m % 10]]
}

fn expect_masks(model: &Model, what: &str, want: &[[u8; 4]]) -> bool {
    let (masks, colon) = clock_masks(model);
    let ok = want.contains(&masks);
    say(&format!(
        "   check {what}: [{}] {}",
        masks_str(masks, colon),
        if ok { "ok" } else { "FAILED" }
    ));
    ok
}

fn expect_time(model: &Model) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // The minute may have turned between the refresh and the readback.
    let want = [utc_hhmm(now), utc_hhmm(now - 60)];
    if std::env::var_os("TZ").is_none() {
        return expect_masks(model, "time (UTC)", &want);
    }
    // Local time under TZ: only the minutes are compared (zone offsets are whole hours or
    // half hours; a half-hour zone fails here, so run without TZ).
    let (masks, colon) = clock_masks(model);
    let ok = want.iter().any(|w| w[2..] == masks[2..]);
    say(&format!(
        "   check time (TZ set, minutes only): [{}] {}",
        masks_str(masks, colon),
        if ok { "ok" } else { "FAILED" }
    ));
    ok
}

fn expect_dark(model: &Model) -> bool {
    let (masks, colon) = clock_masks(model);
    let ok = masks == [0; 4] && !colon;
    say(&format!(
        "   check dark: [{}] {}",
        masks_str(masks, colon),
        if ok { "ok" } else { "FAILED" }
    ));
    ok
}

/// Output level the clock applies for brightness 1 and `ambient`: chip current on the Midi,
/// value of every lit segment on the Lite 2.
fn expect_level(model: &Model, ambient: f32) -> bool {
    let want = ((255.0 * ambient).round() as i32).clamp(2, 255);
    let (got, ok) = if model.clock_chip_current {
        let cur: Vec<i32> = MIDI_CURRENT
            .iter()
            .map(|p| read_trimmed(p).and_then(|v| v.parse().ok()).unwrap_or(-1))
            .collect();
        (format!("current {cur:?}"), cur.iter().all(|&c| c == want))
    } else {
        let lit: Vec<i32> = (0..4)
            .flat_map(|d| (0..7).map(move |s| (d, s)))
            .map(|(d, s)| clock_led(model, &seg_name(model, d, s)))
            .filter(|&v| v > 0)
            .collect();
        let ok = !lit.is_empty() && lit.iter().all(|&v| v == want);
        (format!("lit segments {lit:?}"), ok)
    };
    say(&format!(
        "   check level {want}: {got} {}",
        if ok { "ok" } else { "FAILED" }
    ));
    ok
}

fn with_check(mut s: Step, check: impl Fn(&Model) -> bool + 'static) -> Step {
    s.clock_check = Some(Box::new(check));
    s
}

fn with_half_check(mut s: Step, check: impl Fn(&Model) -> bool + 'static) -> Step {
    s.half_check = Some(Box::new(check));
    s
}

/// The C++ probe's clock steps, plus text, face and code read back.
fn clock_steps(clock: &ClockHandle, music: &Rc<Cell<bool>>, leds: Option<LedsHandle>) -> Vec<Step> {
    // The levels the clock reaches for lux 0, 10, 1000 at brightness 1 (same smoothing).
    let mut dim = AmbientDimmer::new();
    let ambient: Vec<f32> = [0.0, 10.0, 1000.0]
        .iter()
        .map(|&lux| {
            dim.update(lux);
            dim.factor()
        })
        .collect();
    let c = clock.clone();
    let st = move |s: UiState| {
        let c = c.clone();
        move || c.set_ui_state(s)
    };
    let mut steps = Vec::new();
    steps.push(with_check(step("clock time", || {}), expect_time));
    steps.push(with_check(step("clock level at start", || {}), |m| {
        expect_level(m, 1.0)
    }));
    let c = clock.clone();
    // Shown 1.5 s: checked at 1/2 of the slot.
    steps.push(with_half_check(
        step("clock flash_number 42", move || c.flash_number(42)),
        |m| expect_masks(m, "42", &[[0, 0, FONT[4], FONT[2]]]),
    ));
    let c = clock.clone();
    steps.push(step("clock set_timer 125 s", move || {
        c.set_timer(125_000, true)
    }));
    let c = clock.clone();
    let a = ambient[0];
    steps.push(with_check(
        step("clock timer cleared, brightness 1.0 lux 0", move || {
            c.set_timer(0, true);
            c.set_brightness(1.0);
            c.set_ambient_lux(0.0);
        }),
        move |m| expect_level(m, a) & expect_time(m),
    ));
    let c = clock.clone();
    let a = ambient[1];
    steps.push(with_check(
        step("clock lux 10", move || c.set_ambient_lux(10.0)),
        move |m| expect_level(m, a),
    ));
    let c = clock.clone();
    let a = ambient[2];
    steps.push(with_check(
        step("clock lux 1000", move || c.set_ambient_lux(1000.0)),
        move |m| expect_level(m, a),
    ));
    steps.push(step("clock LISTENING", st(UiState::Listening)));
    steps.push(step("clock THINKING", st(UiState::Thinking)));
    steps.push(step("clock SPEAKING", st(UiState::Speaking)));
    let c = clock.clone();
    steps.push(step("clock voice indication off", move || {
        c.set_voice_indication(false)
    }));
    steps.push(step("clock TIMER_RING", st(UiState::TimerRing)));
    let c = clock.clone();
    steps.push(with_check(
        step("clock IDLE", move || {
            c.set_voice_indication(true);
            c.set_ui_state(UiState::Idle);
        }),
        expect_time,
    ));
    for (name, shot) in [
        ("clock BOOT", ClockShot::Boot),
        ("clock PAUSE", ClockShot::Pause),
        ("clock NEXT", ClockShot::Next),
        ("clock PREVIOUS", ClockShot::Previous),
    ] {
        let c = clock.clone();
        steps.push(step(name, move || c.play(shot)));
    }
    let c = clock.clone();
    steps.push(with_check(
        step("clock updating", move || c.set_updating(true)),
        expect_dark,
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock update done", move || c.set_updating(false)),
        expect_time,
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock text 1234", move || c.set_text("1234".into())),
        |m| expect_masks(m, "1234", &[[FONT[1], FONT[2], FONT[3], FONT[4]]]),
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock face Smiley", move || c.set_face(0)),
        |m| expect_masks(m, "Smiley", &[SMILEY]),
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock face cleared, code 123456", move || {
            c.set_face(-1);
            c.show_code("123456".into());
        }),
        // Pages every 2 s: "123-", then "-456".
        |m| {
            expect_masks(
                m,
                "code",
                &[
                    [FONT[1], FONT[2], FONT[3], MINUS],
                    [MINUS, FONT[4], FONT[5], FONT[6]],
                ],
            )
        },
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock code cleared", move || c.clear_code()),
        expect_time,
    ));
    let c = clock.clone();
    let m = Rc::clone(music);
    steps.push(step("clock music Levels", move || {
        m.set(true);
        c.set_media_playing(true);
        c.set_music_style("Levels".into());
    }));
    let c = clock.clone();
    let m = Rc::clone(music);
    steps.push(with_check(
        step("clock music Off", move || {
            m.set(false);
            c.set_music_style("Off".into());
            c.set_media_playing(false);
        }),
        expect_time,
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock disabled", move || c.set_enabled(false)),
        expect_dark,
    ));
    let c = clock.clone();
    steps.push(with_check(
        step("clock enabled, idle off", move || {
            c.set_enabled(true);
            if let Some(l) = &leds {
                l.set_ui_state(UiState::Idle);
            }
        }),
        expect_time,
    ));
    steps
}

/// Midi: the chip currents before the probe, written back at the end (the daemon sets its own
/// level when it starts).
fn save_current(model: &Model) -> Option<[String; 2]> {
    if !model.clock_chip_current {
        return None;
    }
    Some(MIDI_CURRENT.map(|p| read_trimmed(p).unwrap_or_default()))
}

fn restore_current(saved: Option<[String; 2]>) {
    let Some(saved) = saved else { return };
    for (p, v) in MIDI_CURRENT.iter().zip(&saved) {
        if !v.is_empty() {
            pulsar_core::sysfs::write_file(p, v);
        }
    }
    say(&format!("restored clock current {}/{}", saved[0], saved[1]));
}

async fn clock(model: &'static Model, state_ms: u64) -> bool {
    let saved = save_current(model);
    let clock = pulsar_clock::spawn(model);
    let music = Rc::new(Cell::new(false));
    let done = Rc::new(Cell::new(false));
    feed_music(
        None,
        Some(clock.clone()),
        Rc::clone(&music),
        Rc::clone(&done),
    );
    let steps = clock_steps(&clock, &music, None);
    let dumps = Dumps {
        ring: false,
        clock: true,
    };
    let ok = run_steps(model, steps, state_ms, dumps).await;
    say(&format!("final clock {}", clock_dump(model)));
    done.set(true);
    drop(clock);
    let stopped = actors::join_all(Duration::from_secs(3)).await;
    restore_current(saved);
    ok && stopped
}

// -------------------------------------------------------------------------------------------------
// light

async fn light(seconds: u64) -> bool {
    let rx = match pulsar_clock::spawn_light_sensor(1000) {
        Ok(rx) => rx,
        Err(e) => {
            say(&format!("open: light=0 ({e})"));
            return false;
        }
    };
    say(&format!("== monitoring lux for {seconds} s"));
    monitor(None, Some(rx), seconds).await > 0
}

// -------------------------------------------------------------------------------------------------
// all

async fn all(model: &'static Model, input_s: u64, state_ms: u64) -> bool {
    let saved = save_current(model);
    fb_rate();
    let leds = pulsar_hw::spawn_leds(model);
    let clock = pulsar_clock::spawn(model);
    let (input, input_rx): (Option<InputHandle>, _) = match pulsar_hw::spawn_input(model) {
        Ok((h, rx)) => (Some(h), Some(rx)),
        Err(e) => {
            say(&format!("input: {e}"));
            (None, None)
        }
    };
    let light_rx = match pulsar_clock::spawn_light_sensor(1000) {
        Ok(rx) => Some(rx),
        Err(e) => {
            say(&format!("light: {e}"));
            None
        }
    };
    // The handles do not report whether their device opened (the daemon does not need it): the
    // ring's framebuffer or first sysfs LED, and any clock segment, stand in for C++ open().
    let leds_open = std::path::Path::new(model.ring.fb_dev).exists()
        || std::path::Path::new(&format!(
            "/sys/class/leds/{}:red:{}{}/brightness",
            model.led_prefix, model.ring.sysfs_kind, model.ring.first_index
        ))
        .exists();
    let clock_open = (0..4).any(|d| {
        (0..7).any(|s| {
            std::path::Path::new(&format!(
                "{}{}/brightness",
                model.clock_dir,
                seg_name(model, d, s)
            ))
            .exists()
        })
    });
    say(&format!(
        "open: leds={} clock={} input={} light={} hw_muted={}",
        u8::from(leds_open),
        u8::from(clock_open),
        u8::from(input.is_some()),
        u8::from(light_rx.is_some()),
        u8::from(input.as_ref().is_some_and(InputHandle::hw_muted))
    ));
    let music = Rc::new(Cell::new(false));
    let done = Rc::new(Cell::new(false));
    feed_music(
        Some(leds.clone()),
        Some(clock.clone()),
        Rc::clone(&music),
        Rc::clone(&done),
    );
    leds.set_muted(false);
    let mut steps = led_steps(&leds, &music);
    steps.extend(clock_steps(&clock, &music, Some(leds.clone())));
    let dumps = Dumps {
        ring: true,
        clock: true,
    };
    let ok = run_steps(model, steps, state_ms, dumps).await;
    say(&format!(
        "== monitoring input/lux for {input_s} s (press buttons, toggle mute)"
    ));
    // Readings queued during the steps are dropped; the poll goes on every second.
    let mut light_rx = light_rx;
    if let Some(rx) = light_rx.as_mut() {
        while rx.try_recv().is_ok() {}
    }
    let light_ok = light_rx.is_some();
    let readings = monitor(input_rx, light_rx, input_s).await;
    say(&format!("final clock {}", clock_dump(model)));
    done.set(true);
    drop(input);
    drop(clock);
    drop(leds);
    let off = leds_off_after_exit(model).await;
    restore_current(saved);
    ok && off && light_ok && readings > 0
}
