//! Hardware probe, run with the daemon stopped.
//! usage: hw_probe leds [state_ms=2000]   cycle LED states, effects, music styles (synthetic
//!                                        music at 30 fps), dump sysfs; ring dark after exit
//!        hw_probe input [seconds=15]      print buttons and the mute switch
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::io::Write as _;
use std::os::unix::fs::FileExt as _;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pulsar_core::model::{LedStrip, Model};
use pulsar_core::{MusicFrame, UiState, actors};
use pulsar_hw::{Button, InputEvent, LedsHandle, Press, Rgb};
use tokio::task::LocalSet;

const USAGE: &str = "usage: hw_probe leds [state_ms=2000] | input [seconds=15]";

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
    let cmd = args.first().map(String::as_str);
    let (Some("leds" | "input"), Some(n)) =
        (cmd, arg(1, if cmd == Some("leds") { 2000 } else { 15 }))
    else {
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
            Some("leds") => leds(model, n).await,
            _ => input(model, n).await,
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

struct Step {
    name: String,
    run: Box<dyn Fn()>,
    /// Checked at the 3/4 dump.
    check: Option<[u8; 3]>,
}

fn step(name: impl Into<String>, run: impl Fn() + 'static) -> Step {
    Step {
        name: name.into(),
        run: Box::new(run),
        check: None,
    }
}

/// Synthetic music features at 30 frames/s while `on`, until `done`.
fn feed_music(leds: LedsHandle, on: Rc<Cell<bool>>, done: Rc<Cell<bool>>) {
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
            leds.music_frame(f, false);
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

/// Runs step i at t0 + i·state_ms, dumping the ring at 1/2 and 3/4 of its slot (one time base,
/// like the C++ probe's timers); false if a check failed.
async fn run_steps(model: &Model, steps: Vec<Step>, state_ms: u64) -> bool {
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
        say(&format!("   ring {}", ring_dump(model)));
        at(start + state_ms * 3 / 4).await;
        say(&format!("   ring {}", ring_dump(model)));
        if let Some(fb) = s.check {
            ok &= check_strip(model, &model.ring, &s.name, fb);
        }
    }
    at(end).await;
    ok
}

async fn leds(model: &'static Model, state_ms: u64) -> bool {
    fb_rate();
    let leds = pulsar_hw::spawn_leds(model);
    let music = Rc::new(Cell::new(false));
    let done = Rc::new(Cell::new(false));
    feed_music(leds.clone(), Rc::clone(&music), Rc::clone(&done));
    // Initial state, no indication.
    leds.set_muted(false);
    let steps = led_steps(&leds, &music);
    let ok = run_steps(model, steps, state_ms).await;
    // Every handle gone: the actor turns the ring and logo off.
    done.set(true);
    drop(leds);
    let stopped = actors::join_all(Duration::from_secs(3)).await;
    say(&format!("== exit (actors stopped: {stopped})"));
    say(&format!("   ring {}", ring_dump(model)));
    let mut off = check_strip(model, &model.ring, "off after exit", [0; 3]);
    if model.logo.count > 0 {
        off &= check_strip(model, &model.logo, "off after exit", [0; 3]);
    }
    off && stopped && ok
}

// -------------------------------------------------------------------------------------------------
// input

async fn input(model: &'static Model, seconds: u64) -> bool {
    let (input, mut rx) = match pulsar_hw::spawn_input(model) {
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
    let deadline = tokio::time::sleep(Duration::from_secs(seconds));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            () = &mut deadline => break,
            ev = rx.recv() => match ev {
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
                None => break,
            },
        }
    }
    say(&format!("final hw_muted={}", u8::from(input.hw_muted())));
    true
}
