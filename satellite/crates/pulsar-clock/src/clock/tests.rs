use super::*;

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

const NOW: i64 = 1_000_000;

fn wall() -> Wall {
    Wall {
        hour: 12,
        min: 34,
        first_half: true,
    }
}

fn time_frame() -> Frame {
    glyph_frame(font::time_glyphs(12, 34), true)
}

fn text_frame(s: &str) -> Frame {
    let (g, colon) = font::text_glyphs(&font::layout_text(s.as_bytes()), 0, 400);
    glyph_frame(g, colon)
}

fn bands(x: f32) -> MusicFrame {
    MusicFrame {
        bands: [x; 8],
        ..MusicFrame::default()
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn level_math() {
    assert_eq!(level(0.0, 1.0), 0);
    assert_eq!(level(-1.0, 1.0), 0);
    assert_eq!(level(1.0, 1.0), 255);
    assert_eq!(level(0.5, 1.0), 128); // 127.5 rounds away from zero
    assert_eq!(level(1.0, 0.02), 5);
    // Never below the lowest level that still lights while the user wants it on.
    assert_eq!(level(0.001, 0.02), MIN_LEVEL);
    assert_eq!(level(1.0, 2.0), 255);
}

#[test]
fn layer_priority() {
    let mut l = Layers::new();
    assert_eq!(l.compose(NOW, wall), (time_frame(), 0));

    // Music levels: style on, media playing, a fresh frame.
    l.levels_on = true;
    l.media = true;
    l.music_frame(&bands(0.5), false, NOW);
    let (levels, next) = l.compose(NOW, wall);
    assert_eq!(levels, clock_fx::levels(&l.mus_in));
    assert_ne!(levels, Frame::default());
    assert_eq!(next, MUSIC_FRAME_MS);

    l.set_text("Abc", NOW);
    assert_eq!(l.compose(NOW, wall), (text_frame("Abc"), 0));

    l.show_temporary_face(0, 10, NOW);
    let face = glyph_frame(font::face_glyphs(0, 0), false);
    assert_eq!(l.compose(NOW, wall), (face, 10_000));

    l.set_timer(65_000, true, NOW);
    let timer = glyph_frame(font::timer_glyphs(65), true);
    assert_eq!(l.compose(NOW, wall), (timer, 0));

    l.play(ClockShot::Pause, NOW);
    assert_eq!(
        l.compose(NOW, wall),
        (clock_fx::render(Anim::Pause, 0), FX_FRAME_MS)
    );

    l.updating = true;
    assert_eq!(l.compose(NOW, wall), (Frame::default(), 0));

    // LISTENING only with voice indication, TIMER_RING always.
    l.set_voice_indication(false, NOW);
    l.set_ui_state(UiState::Listening, NOW);
    assert_eq!(l.compose(NOW, wall), (Frame::default(), 0));
    l.set_voice_indication(true, NOW);
    assert_eq!(
        l.compose(NOW, wall),
        (clock_fx::render(Anim::Listening, 0), FX_FRAME_MS)
    );
    l.set_voice_indication(false, NOW);
    l.set_ui_state(UiState::TimerRing, NOW);
    assert_eq!(
        l.compose(NOW, wall),
        (clock_fx::render(Anim::Ringing, 0), FX_FRAME_MS)
    );

    l.flash_number(42, NOW);
    let flash = glyph_frame(font::number_glyphs(42), false);
    assert_eq!(l.compose(NOW, wall), (flash, FLASH_MS));

    l.enabled = false;
    assert_eq!(l.compose(NOW, wall), (Frame::default(), 0));

    // The pairing code shows even when disabled.
    l.show_code("1234".into(), NOW);
    let code = glyph_frame(font::code_glyphs(b"1234", 0, CODE_PAGE_MS), false);
    assert_eq!(l.compose(NOW, wall), (code, 0));

    // Peel the layers off again.
    l.code.clear();
    assert_eq!(l.compose(NOW, wall).0, Frame::default());
    l.enabled = true;
    assert_eq!(l.compose(NOW, wall).0, flash);
    l.flash_until = 0;
    assert_eq!(l.compose(NOW, wall).0, clock_fx::render(Anim::Ringing, 0));
    l.set_ui_state(UiState::Idle, NOW);
    assert_eq!(l.compose(NOW, wall).0, Frame::default());
    l.updating = false;
    assert_eq!(l.compose(NOW, wall).0, clock_fx::render(Anim::Pause, 0));
    l.shot = None;
    assert_eq!(l.compose(NOW, wall).0, timer);
    l.set_timer(0, true, NOW);
    assert_eq!(l.compose(NOW, wall).0, face);
    l.show_temporary_face(-1, 0, NOW);
    assert_eq!(l.compose(NOW, wall).0, text_frame("Abc"));
    l.set_text("", NOW);
    l.music_frame(&bands(0.5), false, NOW);
    assert_eq!(l.compose(NOW, wall).0, clock_fx::levels(&l.mus_in));
    l.media = false;
    assert_eq!(l.compose(NOW, wall).0, time_frame());
}

#[test]
fn error_state_shows_nothing_of_its_own() {
    let mut l = Layers::new();
    l.set_ui_state(UiState::Error, NOW);
    assert_eq!(l.compose(NOW, wall).0, time_frame());
    // Changing the state to the same animation keeps its phase.
    l.set_ui_state(UiState::Thinking, NOW);
    l.set_ui_state(UiState::Thinking, NOW + 500);
    assert_eq!(l.voice_t0, NOW);
}

#[test]
fn one_shot_ends_and_frame_steps() {
    let mut l = Layers::new();
    l.play(ClockShot::Next, NOW);
    assert_eq!(l.compose(NOW + 7, wall).1, 13);
    // The last frame step is cut at the end of the animation.
    assert_eq!(l.compose(NOW + 1495, wall).1, 5);
    assert_eq!(l.compose(NOW + 1499, wall).1, 1);
    assert_eq!(l.compose(NOW + 1500, wall), (time_frame(), 0));
    assert_eq!(l.shot, None);
}

#[test]
fn timer_countdown() {
    let mut l = Layers::new();
    l.set_timer(65_000, true, NOW);
    let shows = |l: &mut Layers, at: i64| l.compose(at, wall).0;
    assert_eq!(
        shows(&mut l, NOW),
        glyph_frame(font::timer_glyphs(65), true)
    );
    // Whole seconds are rounded up.
    assert_eq!(
        shows(&mut l, NOW + 1),
        glyph_frame(font::timer_glyphs(65), true)
    );
    assert_eq!(
        shows(&mut l, NOW + 1000),
        glyph_frame(font::timer_glyphs(64), true)
    );
    assert_eq!(
        shows(&mut l, NOW + 64_001),
        glyph_frame(font::timer_glyphs(1), true)
    );
    // At 00:00 the digits and the colon blink every 500 ms (NOW is a multiple of 1000).
    let zero = glyph_frame(font::timer_glyphs(0), true);
    assert_eq!(shows(&mut l, NOW + 65_000), zero);
    assert_eq!(shows(&mut l, NOW + 65_499), zero);
    assert_eq!(shows(&mut l, NOW + 65_500), Frame::default());
    assert_eq!(shows(&mut l, NOW + 66_000), zero);
    assert_eq!(shows(&mut l, NOW + 99_700), Frame::default());

    // Paused: the time left stays.
    l.set_timer(3_600_000, false, NOW);
    assert_eq!(
        shows(&mut l, NOW + 50_000),
        glyph_frame(font::timer_glyphs(3600), true)
    );
    l.set_timer(-5, true, NOW);
    assert_eq!(shows(&mut l, NOW), time_frame());
}

#[test]
fn flash_number_clamps_and_expires() {
    let mut l = Layers::new();
    l.flash_number(123_456, NOW);
    assert_eq!(l.flash_value, 9999);
    l.flash_number(-5000, NOW);
    assert_eq!(l.flash_value, -999);
    assert_eq!(l.compose(NOW + 1000, wall).1, 500);
    assert_eq!(l.compose(NOW + 1500, wall), (time_frame(), 0));
}

#[test]
fn text_and_face_replace_each_other() {
    let mut l = Layers::new();
    l.set_face(3, NOW);
    // Clearing the text keeps the face; text of only ':' replaces it.
    l.set_text("", NOW);
    assert!(matches!(l.base, Some(Content::Face { face: 3, .. })));
    l.set_text(":", NOW);
    assert_eq!(l.base, None);
    l.set_text("hi", NOW);
    l.set_face(-1, NOW);
    assert!(matches!(l.base, Some(Content::Text { .. })));
    l.set_face(font::FACE_COUNT, NOW);
    assert!(matches!(l.base, Some(Content::Text { .. })));
    l.set_face(font::FACE_ALL, NOW);
    assert!(matches!(
        l.base,
        Some(Content::Face {
            face: font::FACE_ALL,
            ..
        })
    ));
    l.set_text("", NOW);
    assert!(l.base.is_some());
}

#[test]
fn moving_content_frame_steps() {
    let mut l = Layers::new();
    l.set_scroll_ms(50);
    assert_eq!(l.scroll_ms, 100);
    l.set_scroll_ms(5000);
    assert_eq!(l.scroll_ms, 2000);
    l.set_scroll_ms(300);
    l.set_text("12345", NOW);
    assert_eq!(l.compose(NOW + 50, wall).1, 250);
    l.set_text("1234", NOW);
    assert_eq!(l.compose(NOW + 50, wall).1, 0);
    l.set_face(font::FACE_BLINK, NOW);
    assert_eq!(l.compose(NOW + 30, wall).1, 70);
    l.set_face(font::FACE_ALL, NOW);
    assert_eq!(l.compose(NOW + 30, wall).1, 1970);
}

#[test]
fn temporary_layer_durations() {
    let mut l = Layers::new();
    l.set_scroll_ms(300);
    // One pass of a scrolling text: (n + 4) steps.
    l.show_temporary_text("123456", 0, NOW);
    assert_eq!(l.overlay_until, NOW + 10 * 300);
    // The frame timer also fires when the layer ends.
    assert_eq!(l.compose(NOW + 2950, wall).1, 50);
    assert_eq!(l.compose(NOW + 3000, wall).0, time_frame());
    assert_eq!(l.overlay, None);
    l.show_temporary_text("12", 0, NOW);
    assert_eq!(l.overlay_until, NOW + TEMPORARY_MS);
    assert_eq!(l.compose(NOW + 4000, wall).1, 1000);
    l.show_temporary_text("12", 7, NOW);
    assert_eq!(l.overlay_until, NOW + 7000);
    l.show_temporary_text("12", 100_000, NOW);
    assert_eq!(l.overlay_until, NOW + 3_600_000);
    l.show_temporary_face(2, -1, NOW);
    assert_eq!(l.overlay_until, NOW + TEMPORARY_MS);
    l.show_temporary_text("", 5, NOW);
    assert_eq!(l.overlay, None);
    // A layer that ends right now still gets a 1 ms frame timer.
    l.show_temporary_face(2, 1, NOW);
    assert_eq!(l.compose(NOW + 999, wall).1, 1);
}

#[test]
fn level_ranges_per_source() {
    let mut l = Layers::new();
    // The first frame of a source starts its range: the minimum span around the value.
    l.music_frame(&bands(0.5), false, NOW);
    assert!(l.mus_in.iter().all(|&v| close(v, 0.5)));
    // Silence stays dark instead of showing the middle of the meter.
    let mut s = Layers::new();
    s.music_frame(&bands(0.0), false, NOW);
    assert_eq!(s.mus_in, [0.0; 4]);

    // Peaks widen the range at once.
    l.music_frame(&bands(0.9), false, NOW + 33);
    assert!(l.mus_in.iter().all(|&v| close(v, 1.0)));
    let r = l.ranges[0];
    assert!(close(r.hi[0], 0.9));
    let k = (-33.0f32 / RANGE_TAU_MS).exp();
    assert!(close(r.lo[0], 0.9 + (0.5 - 0.9) * k));
    // The range closes in with a step of at most 100 ms across a gap, and survives a pause.
    l.music_frame(&bands(0.7), false, NOW + 33 + 9_999);
    let k = (-100.0f32 / RANGE_TAU_MS).exp();
    let r2 = l.ranges[0];
    assert!(close(r2.hi[0], 0.7 + (0.9 - 0.7) * k));
    assert!(close(r2.lo[0], 0.7 + (r.lo[0] - 0.7) * k));
    let span = r2.hi[0] - r2.lo[0];
    assert!(close(l.mus_in[0], (0.7 - r2.lo[0]) / span));
    // 10 s without the source: it starts over.
    l.music_frame(&bands(0.2), false, NOW + 33 + 9_999 + RANGE_KEEP_MS);
    assert!(close(l.ranges[0].lo[0], 0.2) && close(l.ranges[0].hi[0], 0.2));

    // Bands are averaged pairwise per digit.
    let mut l = Layers::new();
    let f = MusicFrame {
        bands: [0.0, 0.2, 0.4, 0.4, 0.6, 0.6, 1.0, 1.0],
        ..MusicFrame::default()
    };
    l.music_frame(&f, true, NOW);
    let r = l.ranges[1];
    assert!(close(r.lo[0], 0.1) && close(r.lo[1], 0.4) && close(r.lo[3], 1.0));
    // A full-scale value keeps the span inside 0..1.
    assert!(close(l.mus_in[3], 1.0));
    assert_eq!(l.ranges[0].last_ms, i64::MIN / 2);
}

#[test]
fn sendspin_frames_win_while_fresh() {
    let mut l = Layers::new();
    l.music_frame(&bands(0.5), true, NOW);
    let shown = l.mus_in;
    l.music_frame(&bands(0.0), false, NOW + SENDSPIN_FRESH_MS - 1);
    assert_eq!(l.mus_in, shown);
    assert_eq!(l.mus_in_ms, NOW);
    assert_eq!(l.ranges[0].last_ms, i64::MIN / 2);
    l.music_frame(&bands(0.0), false, NOW + SENDSPIN_FRESH_MS);
    assert_eq!(l.mus_in, [0.0; 4]);
    assert_eq!(l.mus_in_ms, NOW + SENDSPIN_FRESH_MS);
}

#[test]
fn meters_cycle_with_the_time_and_fall() {
    let mut l = Layers::new();
    l.levels_on = true;
    l.media = true;
    let feed = |l: &mut Layers, at: i64, x: f32| {
        l.ranges = [Range::default(); 2];
        l.music_frame(&bands(x), false, at);
        l.mus_in = [x; 4];
    };
    feed(&mut l, NOW, 0.8);
    assert_eq!(l.compose(NOW, wall).0, clock_fx::levels(&[0.8; 4]));
    assert_eq!(l.viz_t0, NOW);
    // The meters fall with a 250 ms time constant.
    feed(&mut l, NOW + 250, 0.0);
    let k = (-1.0f32).exp();
    assert_eq!(
        l.compose(NOW + 250, wall).0,
        clock_fx::levels(&[0.8 * k; 4])
    );
    // Rises show at once.
    feed(&mut l, NOW + 290, 0.9);
    assert_eq!(l.compose(NOW + 290, wall).0, clock_fx::levels(&[0.9; 4]));
    // The time takes over for the last 3 s of every 30 s.
    feed(&mut l, NOW + 26_999, 0.9);
    assert_ne!(l.compose(NOW + 26_999, wall).0, time_frame());
    feed(&mut l, NOW + 27_000, 0.9);
    assert_eq!(l.compose(NOW + 27_000, wall), (time_frame(), 0));
    feed(&mut l, NOW + 30_000, 0.9);
    assert_eq!(l.compose(NOW + 30_000, wall).1, MUSIC_FRAME_MS);
    // Stale frames end the display; the next start resets the meters and the cycle.
    assert_eq!(
        l.compose(NOW + 30_000 + MUSIC_STALE_MS, wall).0,
        time_frame()
    );
    assert!(!l.viz);
    feed(&mut l, NOW + 40_000, 0.3);
    assert_eq!(l.compose(NOW + 40_000, wall).0, clock_fx::levels(&[0.3; 4]));
    assert_eq!(l.viz_t0, NOW + 40_000);
    // Media stopped.
    l.media = false;
    assert_eq!(l.compose(NOW + 40_010, wall).0, time_frame());
}

// --- Output on a fake sysfs tree ---

/// A per-test directory under the system temp dir, removed on drop.
struct Root(PathBuf);

impl Root {
    fn led(&self, prefix: &str, name: &str) -> PathBuf {
        let d = self.0.join(format!("{prefix}{name}"));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("brightness");
        std::fs::write(&f, "").unwrap();
        f
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_root(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("pulsar-clock-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn base_model(compatible: &[u8]) -> &'static Model {
    pulsar_core::model::model_from_compatible(compatible).unwrap()
}

/// A stock model whose clock LEDs live under a fresh temp dir ("<root>/<prefix><led>"); one
/// static pair per call site, since `spawn` takes a `&'static Model`.
macro_rules! temp_model {
    ($name:literal, $compatible:literal, $prefix:literal) => {{
        static DIR: LazyLock<String> =
            LazyLock::new(|| format!("{}/{}", temp_root($name).display(), $prefix));
        static MODEL: LazyLock<Model> = LazyLock::new(|| Model {
            clock_dir: DIR.as_str(),
            ..*base_model($compatible)
        });
        let model: &'static Model = &MODEL;
        let root = Root(Path::new(DIR.as_str()).parent().unwrap().to_owned());
        (model, root)
    }};
}

/// Contents written since the last call, which empties the file again; "" = no write.
fn take(path: &Path) -> String {
    let s = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, "").unwrap();
    s
}

/// Every clock LED of `m` (Lite 2 layout) created; the segment files per digit.
fn create_lite2(m: &Model, root: &Root) -> Vec<Vec<PathBuf>> {
    let segs = (0..DIGITS)
        .map(|d| {
            (0..SEGS)
                .map(|s| root.led("clock", &Leds::seg_name(m, d, s)))
                .collect()
        })
        .collect();
    for name in m
        .clock_colon
        .iter()
        .chain(m.clock_aux)
        .chain([&m.clock_center])
    {
        root.led("clock", name);
    }
    segs
}

#[test]
fn writes_only_changes_and_rescales_on_level() {
    let (m, root) = temp_model!("writes", b"yandex,plum\0", "clock");
    let segs = create_lite2(m, &root);
    let mut leds = Leds::open(m).unwrap();
    let led = |name: &str| root.0.join(format!("clock{name}/brightness"));
    // Aux LEDs are switched off once.
    assert_eq!(take(&led("DEGREE")), "0");
    assert_eq!(take(&led("H2")), "0");

    let one = glyph_frame([0, 0, 0, 0x06], true);
    leds.draw(&one);
    assert_eq!(take(&segs[3][1]), "255");
    assert_eq!(take(&segs[3][0]), "0");
    assert_eq!(take(&led("DH")), "255");
    assert_eq!(take(&led("DL")), "255");
    assert_eq!(take(&led("DM")), "0");
    // Unchanged: nothing written.
    leds.draw(&one);
    assert_eq!(take(&segs[3][1]), "");
    assert_eq!(take(&led("DH")), "");
    // The full rewrite.
    leds.forget_written();
    leds.draw(&one);
    assert_eq!(take(&segs[3][1]), "255");
    assert_eq!(take(&segs[0][0]), "0");

    // No chip current on the Lite 2: the level is the value of a lit segment.
    assert!(leds.set_level(level(1.0, 0.02)));
    assert!(!leds.set_level(5));
    leds.draw(&one);
    assert_eq!(take(&segs[3][1]), "5");
    assert_eq!(take(&segs[3][0]), "0");
    let half = Frame {
        center: 128,
        ..Frame::default()
    };
    leds.draw(&half);
    assert_eq!(take(&led("DM")), "3"); // (128 * 5 + 127) / 255
}

#[test]
fn missing_leds_and_chip_current_dedup() {
    let (m, root) = temp_model!("dedup", b"yandex,cucumber\0", "clock-");
    // No segment at all: unavailable.
    assert!(Leds::open(m).is_none());
    assert!(!panel_present(m));

    for d in 0..DIGITS {
        for s in 0..SEGS {
            root.led("clock-", &Leds::seg_name(m, d, s));
        }
    }
    assert!(panel_present(m));
    let chip_a = root.0.join("chip-a");
    let chip_b = root.0.join("chip-b");
    for c in [&chip_a, &chip_b] {
        std::fs::create_dir_all(c).unwrap();
        std::fs::write(c.join("current"), "").unwrap();
    }
    let link = |seg: &str, chip: &Path| {
        std::os::unix::fs::symlink(chip, root.0.join(format!("clock-{seg}/device"))).unwrap();
    };
    // Both outer digits on one chip: its current once.
    link("a1", &chip_a);
    link("a4", &chip_a);
    let leds = Leds::open(m).unwrap();
    assert_eq!(leds.current.len(), 1);
    assert!(leds.colon.is_empty());

    std::fs::remove_file(root.0.join("clock-a4/device")).unwrap();
    link("a4", &chip_b);
    let mut leds = Leds::open(m).unwrap();
    assert_eq!(leds.current.len(), 2);
    assert!(leds.set_level(77));
    assert_eq!(take(&chip_a.join("current")), "77");
    assert_eq!(take(&chip_b.join("current")), "77");
    // Segments stay at full value; the chips dim.
    leds.draw(&glyph_frame([0x01, 0, 0, 0], false));
    assert_eq!(take(&root.0.join("clock-a1/brightness")), "255");
}

#[tokio::test]
async fn actor_draws_commands_and_stops_with_the_handles() {
    let (m, root) = temp_model!("actor", b"yandex,plum\0", "clock");
    let segs = create_lite2(m, &root);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let clock = spawn(m);
            let other = clock.clone();
            clock.set_text("1234".into());
            other.show_code("8".into());
            other.clear_code();
            tokio::time::sleep(Duration::from_millis(30)).await;
            let lit = |d: usize| -> u8 {
                (0..SEGS)
                    .map(|s| {
                        let v = std::fs::read_to_string(&segs[d][s]).unwrap();
                        u8::from(v.starts_with("255")) << s
                    })
                    .sum()
            };
            let want = font::text_glyphs(&font::layout_text(b"1234"), 0, 400).0;
            assert_eq!([lit(0), lit(1), lit(2), lit(3)], want);
            drop(clock);
            drop(other);
            assert!(actors::join_all(Duration::from_secs(1)).await);
        })
        .await;
}

#[tokio::test]
async fn auto_brightness_off_ignores_the_light() {
    // Midi layout: the level is the chips' current, written only when it changes (the periodic
    // full rewrite covers the segments alone).
    let (m, root) = temp_model!("auto_bri", b"yandex,cucumber\0", "clock-");
    for d in 0..DIGITS {
        for s in 0..SEGS {
            root.led("clock-", &Leds::seg_name(m, d, s));
        }
    }
    let chip = root.0.join("chip");
    std::fs::create_dir_all(&chip).unwrap();
    let current = chip.join("current");
    std::fs::write(&current, "").unwrap();
    std::os::unix::fs::symlink(&chip, root.0.join("clock-a1/device")).unwrap();
    // The next level the actor writes while this task yields.
    let next = || async {
        for _ in 0..200 {
            let v = std::fs::read_to_string(&current).unwrap();
            if !v.is_empty() {
                std::fs::write(&current, "").unwrap();
                return v;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("no level written");
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let clock = spawn(m);
            assert_eq!(take(&current), "255");
            // Dark room, auto brightness on: 0.02 of full.
            clock.set_ambient_lux(0.0);
            assert_eq!(next().await, "5");
            clock.set_auto_brightness(false);
            assert_eq!(next().await, "255");
            // A reading while off writes nothing but still feeds the dimmer: switching back on
            // applies the new light.
            let mut dimmer = AmbientDimmer::new();
            dimmer.update(0.0);
            assert!(dimmer.update(1000.0));
            let (half, half_lit) = (level(0.5, 1.0), level(0.5, dimmer.factor()));
            assert!(![5, 255].contains(&half) && ![3, 5, 255, half].contains(&half_lit));
            clock.set_ambient_lux(1000.0);
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert_eq!(take(&current), "");
            clock.set_brightness(0.5);
            assert_eq!(next().await, half.to_string());
            clock.set_auto_brightness(true);
            assert_eq!(next().await, half_lit.to_string());
            drop(clock);
            assert!(actors::join_all(Duration::from_secs(1)).await);
        })
        .await;
}

#[tokio::test]
async fn unavailable_clock_is_a_no_op() {
    let (m, _root) = temp_model!("none", b"yandex,plum\0", "none-");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let clock = spawn(m);
            clock.set_text("1234".into());
            clock.play(ClockShot::Boot);
            assert!(actors::join_all(Duration::from_secs(1)).await);
        })
        .await;
}

#[test]
fn names() {
    assert_eq!(music_styles(), ["Off", "Levels"]);
    assert_eq!(face_names().len(), 19);
}
