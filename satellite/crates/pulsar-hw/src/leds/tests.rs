use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tokio::task::{JoinHandle, LocalSet};

use super::*;

const RED: Rgb = Rgb { r: 255, g: 0, b: 0 };

#[test]
fn to_px_gamma_dim_cap() {
    let white = Col {
        r: 1.0,
        g: 1.0,
        b: 1.0,
    };
    assert_eq!(to_px(white, 1.0, None), [204, 204, 204]);
    // Gamma 2 after dimming; out-of-range inputs clamp first.
    let c = Col {
        r: 0.5,
        g: 2.0,
        b: -1.0,
    };
    assert_eq!(to_px(c, 1.0, None), [51, 204, 0]);
    assert_eq!(to_px(white, 0.5, None), [51, 51, 51]);
    // Ambient floor: 0.3 in the dark.
    let mut comp = Compositor::new(3, 0);
    assert!((comp.dim() - 1.0).abs() < 1e-6);
    comp.dimmer.update(0.0);
    assert!((comp.dim() - 0.3).abs() < 1e-6);
}

#[test]
fn to_px_case_matrix_keeps_hue_when_clipping() {
    let white = Col {
        r: 1.0,
        g: 1.0,
        b: 1.0,
    };
    let id: Matrix = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    assert_eq!(to_px(white, 1.0, Some(&id)), [204, 204, 204]);
    // Rows are output R, G, B; a row above 1 scales the whole pixel down.
    let warm: Matrix = [[2.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    assert_eq!(to_px(white, 1.0, Some(&warm)), [204, 102, 102]);
    // Negative results clamp to 0.
    let neg: Matrix = [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 0.0, 0.5]];
    assert_eq!(to_px(white, 1.0, Some(&neg)), [204, 0, 102]);
}

#[test]
fn matrix_parser_takes_the_key_and_nine_numbers() {
    let stock =
        r#"{"type": "matrix", "matrix": [[0.95, 0.05, 0], [0.0, .9, -0.1], [1e-1, 0.2, 7.5E-1]]}"#;
    assert_eq!(
        parse_matrix(stock),
        Some([[0.95, 0.05, 0.0], [0.0, 0.9, -0.1], [0.1, 0.2, 0.75]])
    );
    // Whitespace before the colon; a "matrix" value is not the key.
    let spaced = "{\"name\": \"matrix\", \"matrix\"\n\t : [1,2,3,4,5,6,7,8,9,10]}";
    assert_eq!(
        parse_matrix(spaced),
        Some([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0]])
    );
    // Fewer than nine numbers before the closing brace.
    assert_eq!(
        parse_matrix(r#"{"matrix": [[1,2,3],[4,5,6]]} [7,8,9]"#),
        None
    );
    // A lone '-' or '.' is no number; NaN / infinity are rejected.
    assert_eq!(parse_matrix(r#"{"matrix": [- 1,2,3,4,5,6,7,8,9]}"#), None);
    assert_eq!(parse_matrix(r#"{"matrix": [-nan,2,3,4,5,6,7,8,9]}"#), None);
    assert_eq!(parse_matrix(r#"{"matrix": [1e999,2,3,4,5,6,7,8,9]}"#), None);
    assert_eq!(parse_matrix(r#"{"type": "matrix"}"#), None);
    assert_eq!(parse_matrix(r#"{"matrix": 5}"#), None);
}

#[test]
fn strtod_prefix_reads_like_c() {
    let p = |s: &str| strtod_prefix(s.as_bytes());
    assert_eq!(p("1.5,"), Some((1.5, 3)));
    assert_eq!(p("-.25]"), Some((-0.25, 4)));
    assert_eq!(p("3."), Some((3.0, 2)));
    assert_eq!(p("2e3x"), Some((2000.0, 3)));
    // An exponent without digits is not part of the number.
    assert_eq!(p("2e+x"), Some((2.0, 1)));
    assert_eq!(p("0x1.8p1"), Some((3.0, 7)));
    assert_eq!(p("0x"), Some((0.0, 1)));
    assert_eq!(p("-"), None);
    assert_eq!(p("."), None);
    assert_eq!(p("-inf"), Some((f64::NEG_INFINITY, 4)));
    assert!(p("nan(1)").is_some_and(|(v, n)| v.is_nan() && n == 6));
}

#[test]
fn case_colors_are_sorted_json_stems() {
    let dir = std::env::temp_dir().join(format!("pulsar-hw-case-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for f in [
        "white.json",
        "black.json",
        ".json",
        "notes.txt",
        "beige.json",
    ] {
        std::fs::write(dir.join(f), "{}").unwrap();
    }
    let got = case_colors_in(&format!("{}/", dir.display()));
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(got, ["beige", "black", "white"]);
    assert!(case_colors_in("/nonexistent/").is_empty());
}

fn ui(c: &mut Compositor, s: UiState, now: i64) -> bool {
    c.apply(Cmd::UiState(s), now)
}

fn user(on: bool) -> Cmd {
    Cmd::UserLight {
        on,
        color: RED,
        brightness: 1.0,
        effect: String::new(),
        transition_ms: 0,
    }
}

#[test]
fn layer_priority() {
    let mut c = Compositor::new(3, 0);
    assert_eq!(c.active_layer(0), Layer::Off);
    c.apply(Cmd::MediaPlaying(true), 0);
    assert_eq!(c.active_layer(0), Layer::Off, "music style Off");
    c.apply(Cmd::MusicStyle("Disco".into()), 0);
    assert_eq!(c.active_layer(0), Layer::Music);
    c.apply(user(true), 0);
    assert_eq!(c.active_layer(0), Layer::User);
    // The first mute call only sets the state; on the Lite 2 the steady mute beats the light.
    c.apply(Cmd::Muted(true), 0);
    assert_eq!(c.active_layer(0), Layer::Mute);
    c.apply(
        Cmd::ShowTemporary {
            effect: "Comet".into(),
            color: RED,
            seconds: 3,
        },
        0,
    );
    assert_eq!(c.active_layer(0), Layer::Show);
    c.apply(Cmd::TimerStarted, 0);
    assert_eq!(c.active_layer(0), Layer::OneShot);
    // Listening shows only with voice indication; Error and TimerRing always.
    c.apply(Cmd::VoiceIndication(false), 0);
    ui(&mut c, UiState::Listening, 0);
    assert_eq!(c.active_layer(0), Layer::OneShot);
    c.apply(Cmd::VoiceIndication(true), 0);
    assert_eq!(c.active_layer(0), Layer::Voice);
    c.apply(Cmd::VoiceIndication(false), 0);
    ui(&mut c, UiState::TimerRing, 0);
    assert_eq!(c.active_layer(0), Layer::Voice);
    c.apply(Cmd::Volume(0.4), 0);
    assert_eq!(c.active_layer(0), Layer::Volume);
    assert_eq!(c.active_layer(VOLUME_SHOW_MS), Layer::Voice);
    // Zero brightness hides the light.
    let mut c = Compositor::new(24, 6);
    c.apply(
        Cmd::UserLight {
            on: true,
            color: RED,
            brightness: 0.0,
            effect: String::new(),
            transition_ms: 0,
        },
        0,
    );
    assert_eq!(c.active_layer(0), Layer::Off);
    // The Midi has no steady mute layer (its logo shows it).
    c.apply(Cmd::Muted(true), 0);
    assert_eq!(c.active_layer(0), Layer::Off);
}

#[test]
fn timed_content_expires() {
    let mut c = Compositor::new(24, 6);
    assert!(ui(&mut c, UiState::Error, 1000));
    // Idle during the error is ignored; it runs its 3.54 s to the end.
    assert!(!ui(&mut c, UiState::Idle, 4539));
    assert_eq!(c.next_expiry(4539), Some(4540));
    c.expire(4540);
    assert_eq!(c.state, UiState::Idle);
    // Another active state replaces it at once.
    ui(&mut c, UiState::Error, 5000);
    assert!(ui(&mut c, UiState::Listening, 5001));

    let mut c = Compositor::new(24, 6);
    c.apply(Cmd::Muted(false), 0);
    assert_eq!(c.sweep, Sweep::None, "first call: no one-shot");
    c.apply(Cmd::TimerStarted, 100);
    c.apply(Cmd::Muted(true), 1000);
    assert_eq!(c.sweep, Sweep::MuteOn);
    c.apply(Cmd::Volume(0.5), 1000);
    c.apply(
        Cmd::ShowTemporary {
            effect: String::new(),
            color: RED,
            seconds: 0,
        },
        1000,
    );
    assert_eq!(c.next_expiry(1000), Some(1000 + fx::MUTE_SWEEP_MS));
    c.expire(1600);
    assert_eq!(c.sweep, Sweep::None);
    // The countdown went on under the sweep.
    assert_eq!(c.active_layer(2600), Layer::OneShot);
    assert_eq!(c.next_expiry(2600), Some(6000), "seconds <= 0: 5 s");
    c.expire(6000);
    assert!(!c.show_on);
    assert_eq!(c.next_expiry(6000), Some(15100));
    c.expire(15100);
    assert!(!c.timer_shot);
    assert_eq!(c.next_expiry(15100), None);
    c.apply(
        Cmd::ShowTemporary {
            effect: String::new(),
            color: RED,
            seconds: 99_999,
        },
        0,
    );
    assert_eq!(c.show_until, 3_600_000);

    // Lite 2 unmute: no sweep, and a running mute flash ends with it.
    let mut c = Compositor::new(3, 0);
    c.apply(Cmd::Muted(false), 0);
    c.apply(Cmd::Muted(true), 10);
    assert_eq!(c.sweep, Sweep::MuteOn);
    c.apply(Cmd::Muted(false), 20);
    assert_eq!(c.sweep, Sweep::None);
}

#[test]
fn midi_logo_follows_voice_mute_or_ring_average() {
    let mut c = Compositor::new(24, 6);
    c.apply(user(true), 0);
    c.frame(0);
    let red = to_px(fx::rgb(255, 0, 0), c.dim(), None);
    assert_eq!(&c.px[24 * 3..24 * 3 + 3], &red, "logo = ring average");
    c.apply(Cmd::Muted(false), 1);
    c.apply(Cmd::Muted(true), 2);
    c.frame(1000);
    assert_eq!(
        &c.px[24 * 3..24 * 3 + 3],
        &to_px(fx::mute_logo(), c.dim(), None)
    );
    ui(&mut c, UiState::Listening, 1000);
    // The voice layer fades in over 250 ms.
    c.frame(2000);
    c.frame(2300);
    let mut logo = [Col::default(); 6];
    fx::voice_logo(fx::Voice::Listening, 1300, &mut logo);
    assert_eq!(&c.px[24 * 3..24 * 3 + 3], &to_px(logo[0], c.dim(), None));
}

#[test]
fn layer_changes_fade_but_volume_cuts_in() {
    let mut c = Compositor::new(3, 0);
    c.frame(0);
    c.apply(
        Cmd::ShowTemporary {
            effect: String::new(),
            color: RED,
            seconds: 10,
        },
        0,
    );
    assert!(c.frame(0), "fading");
    assert_eq!(&c.px[..3], &[0, 0, 0]);
    c.frame(125);
    let half = to_px(
        fx::lerp(Col::default(), fx::rgb(255, 0, 0), 0.5),
        c.dim(),
        None,
    );
    assert_eq!(&c.px[..3], &half);
    assert!(!c.frame(250), "fade done, solid colour does not move");
    c.apply(Cmd::Volume(1.0), 300);
    c.frame(300);
    let mut vol = [Col::default(); 3];
    fx::volume(1.0, &mut vol);
    assert_eq!(&c.px[..3], &to_px(vol[0], c.dim(), None));
}

/// The actor on a capture sink.
fn spawn_capture(n: usize, nlogo: usize) -> (LedsHandle, Captured, JoinHandle<()>) {
    let frames = Rc::new(RefCell::new(Vec::new()));
    let out = Output::new(Sink::Capture(Rc::clone(&frames)), n, nlogo);
    let (tx, rx) = unbounded_channel();
    let task = tokio::task::spawn_local(run(Compositor::new(n, nlogo), out, rx, true));
    (LedsHandle { tx }, frames, task)
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[tokio::test(start_paused = true)]
async fn error_runs_out_then_the_ring_goes_dark_and_frames_stop() {
    LocalSet::new()
        .run_until(async {
            let (leds, frames, task) = spawn_capture(24, 6);
            leds.set_ui_state(UiState::Error);
            sleep_ms(1000).await;
            leds.set_ui_state(UiState::Idle);
            sleep_ms(1500).await;
            let lit = |px: &[u8]| px.iter().any(|v| *v != 0);
            assert!(
                frames.borrow().iter().any(|(t, px)| *t > 2000 && lit(px)),
                "still red after the ignored Idle"
            );
            sleep_ms(1500).await;
            let count = frames.borrow().len();
            let (t, px) = frames.borrow().last().cloned().unwrap();
            assert!(!lit(&px));
            assert!(t >= 3540 && t < 3540 + FADE_MS + 2 * FRAME_MS as i64);
            sleep_ms(5000).await;
            assert_eq!(frames.borrow().len(), count, "no frames while static");
            drop(leds);
            task.await.unwrap();
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn volume_overlay_cuts_in_and_its_expiry_restores_the_light() {
    LocalSet::new()
        .run_until(async {
            let (leds, frames, task) = spawn_capture(3, 0);
            let dim = Compositor::new(3, 0).dim();
            let red = to_px(fx::rgb(255, 0, 0), dim, None).repeat(3);
            leds.set_user_light(true, RED, 1.0, String::new(), 0);
            sleep_ms(500).await;
            assert_eq!(frames.borrow().last().unwrap().1, red, "transition 0: cut");
            leds.show_volume(0.5);
            sleep_ms(10).await;
            let mut vol = [Col::default(); 3];
            fx::volume(0.5, &mut vol);
            let (t0, px) = frames.borrow().last().cloned().unwrap();
            assert_eq!(px, to_px(vol[0], dim, None).repeat(3));
            sleep_ms(2000).await;
            let f = frames.borrow().clone();
            let (t, px) = f.last().unwrap();
            assert_eq!(*px, red);
            // The expiry timer woke the static overlay up after 1.5 s (the fade's first frame
            // still equals the overlay), then the 250 ms fade.
            let first_after = f.iter().find(|(t, _)| *t > t0).unwrap().0;
            assert_eq!(first_after, t0 + VOLUME_SHOW_MS + FRAME_MS as i64);
            assert!(*t <= t0 + VOLUME_SHOW_MS + FADE_MS + FRAME_MS as i64);
            drop(leds);
            task.await.unwrap();
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn exit_clears_and_case_matrix_rewrites() {
    LocalSet::new()
        .run_until(async {
            let (leds, frames, task) = spawn_capture(3, 0);
            leds.set_user_light(true, RED, 1.0, String::new(), 0);
            sleep_ms(500).await;
            let before = frames.borrow().len();
            // Same pixels under the identity matrix: written again anyway.
            leds.send(Cmd::CaseMatrix(Some([
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
            ])));
            sleep_ms(10).await;
            assert_eq!(frames.borrow().len(), before + 1);
            let clone = leds.clone();
            drop(leds);
            sleep_ms(10).await;
            assert!(!task.is_finished(), "a clone keeps it running");
            drop(clone);
            task.await.unwrap();
            assert_eq!(frames.borrow().last().unwrap().1, [0; 9]);
        })
        .await;
}
