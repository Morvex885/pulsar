use super::*;

fn lum(c: Col) -> f32 {
    c.r + c.g + c.b
}

fn contour_leds() -> Vec<usize> {
    (0..PANEL_LEDS)
        .filter(|&i| GEOMETRY[i].contour > 0.5)
        .collect()
}

#[test]
fn contour_is_the_ring_of_fourteen_leds_around_the_logo() {
    let on = contour_leds();
    assert_eq!(on.len(), 14, "{on:?}");
    for &i in &on {
        let (col, row) = PANEL_XY[i];
        assert!(
            (2..=12).contains(&col) && (4..=8).contains(&row),
            "{i}: {col},{row}"
        );
    }
}

#[test]
fn contour_positions_go_clockwise_from_top_centre() {
    let s = |x: f32, y: f32| contour_pos(x, y) / PERIMETER;
    assert!(s(0.0, -2.0).abs() < 1e-6);
    assert!(s(2.0, -2.0) < s(5.0, 0.0));
    assert!(s(5.0, 0.0) < s(0.0, 2.0));
    assert!(s(0.0, 2.0) < s(-5.0, 0.0));
    assert!(s(-5.0, 0.0) < s(-2.0, -2.0));
    assert!(s(-2.0, -2.0) < 1.0);
}

#[test]
fn nothing_is_drawn_on_the_logo() {
    let dark = |out: &[Col]| out[PANEL_LEDS..].iter().all(|c| *c == Col::default());
    let mut out = vec![fx::rgb(9, 9, 9); 88];
    for v in [
        fx::Voice::Listening,
        fx::Voice::Thinking,
        fx::Voice::Speaking,
        fx::Voice::Error,
        fx::Voice::TimerRing,
    ] {
        for t in [0, 200, 700, 3000] {
            out.fill(fx::rgb(9, 9, 9));
            voice(v, t, &mut out);
            assert!(dark(&out), "{v:?} {t}");
        }
    }
    out.fill(fx::rgb(9, 9, 9));
    volume(0.5, &mut out);
    assert!(dark(&out));
    out.fill(fx::rgb(9, 9, 9));
    timer_started(1000, &mut out);
    assert!(dark(&out));
    out.fill(fx::rgb(9, 9, 9));
    mute_sweep(true, 300, &mut out);
    assert!(dark(&out));
    let st = fx::MusicState::default();
    let inp = fx::MusicInput {
        bands: [0.8; 8],
        loudness: 0.8,
        flash: 1.0,
    };
    for s in fx::Style::ALL {
        out.fill(fx::rgb(9, 9, 9));
        music(s, &st, &inp, 3.0, &mut out);
        assert!(dark(&out), "{s:?}");
    }
}

#[test]
fn thinking_arc_goes_around_the_contour() {
    // Голова дуги за полоборота (750 мс) уходит с верхней середины на нижнюю.
    let brightest = |t: i64| {
        let mut out = vec![Col::default(); 88];
        voice(fx::Voice::Thinking, t, &mut out);
        let i = (0..PANEL_LEDS)
            .max_by(|&a, &b| lum(out[a]).total_cmp(&lum(out[b])))
            .unwrap();
        GEOMETRY[i].dy
    };
    assert!(brightest(0) < 0.0);
    assert!(brightest(750) > 0.0);
}

#[test]
fn volume_fills_from_the_left() {
    let mut out = vec![Col::default(); 88];
    volume(0.3, &mut out);
    let side = |left: bool| {
        contour_leds()
            .into_iter()
            .filter(|&i| {
                if left {
                    GEOMETRY[i].dx < -3.5
                } else {
                    GEOMETRY[i].dx > 3.5
                }
            })
            .map(|i| lum(out[i]))
            .fold(0.0f32, f32::max)
    };
    assert!(side(true) > 3.0 * side(false));
}

#[test]
fn mute_sweep_starts_at_the_top() {
    let mut out = vec![Col::default(); 88];
    mute_sweep(true, 150, &mut out);
    let lit: Vec<f32> = contour_leds()
        .into_iter()
        .filter(|&i| lum(out[i]) > 0.1)
        .map(|i| GEOMETRY[i].dy)
        .collect();
    assert!(!lit.is_empty());
    assert!(lit.iter().all(|&dy| dy < 0.0), "{lit:?}");
}

#[test]
fn volume_edges_are_empty_and_full() {
    let mut out = vec![Col::default(); 88];
    volume(0.0, &mut out);
    let full = lum(volume_colour(0.0));
    assert!(contour_leds().iter().all(|&i| lum(out[i]) < 0.15 * full));
    volume(1.0, &mut out);
    let full = lum(volume_colour(1.0));
    assert!(contour_leds().iter().all(|&i| lum(out[i]) > 0.95 * full));
}

#[test]
fn mute_off_sweep_starts_at_the_bottom() {
    let mut out = vec![Col::default(); 88];
    mute_sweep(false, 150, &mut out);
    let lit: Vec<f32> = contour_leds()
        .into_iter()
        .filter(|&i| lum(out[i]) > 0.1)
        .map(|i| GEOMETRY[i].dy)
        .collect();
    assert!(!lit.is_empty());
    assert!(lit.iter().all(|&dy| dy > 0.0), "{lit:?}");
}

#[test]
fn timer_started_shrinks_to_the_top_and_ends_dark() {
    let lit_dy = |t: i64| {
        let mut out = vec![Col::default(); 88];
        timer_started(t, &mut out);
        contour_leds()
            .into_iter()
            .filter(|&i| lum(out[i]) > 0.1)
            .map(|i| GEOMETRY[i].dy)
            .collect::<Vec<f32>>()
    };
    assert_eq!(lit_dy(0).len(), 14);
    // В начале весь контур — на полной яркости, включая самый дальний от верха светодиод.
    let mut out = vec![Col::default(); 88];
    timer_started(0, &mut out);
    let full = lum(fx::stock(fx::TIMER_START_GREEN, 1.0));
    assert!(contour_leds().iter().all(|&i| lum(out[i]) > 0.99 * full));
    let late = lit_dy(fx::TIMER_STARTED_MS * 4 / 5);
    assert!(
        !late.is_empty() && late.iter().all(|&dy| dy < 0.0),
        "{late:?}"
    );
    assert!(lit_dy(fx::TIMER_STARTED_MS).is_empty());
}

#[test]
fn listening_rises_from_the_bottom() {
    let mut out = vec![Col::default(); 88];
    voice(fx::Voice::Listening, 100, &mut out);
    let lit: Vec<f32> = (0..PANEL_LEDS)
        .filter(|&i| lum(out[i]) > 0.05)
        .map(|i| GEOMETRY[i].dy)
        .collect();
    assert!(!lit.is_empty());
    assert!(lit.iter().all(|&dy| dy > 0.0), "{lit:?}");
    voice(fx::Voice::Listening, 1000, &mut out);
    assert!(contour_leds().iter().all(|&i| lum(out[i]) > 0.3));
}

#[test]
fn error_goes_dark_after_its_three_flashes() {
    let mut out = vec![Col::default(); 88];
    voice(fx::Voice::Error, 40, &mut out);
    assert!(contour_leds().iter().all(|&i| lum(out[i]) > 0.0));
    voice(fx::Voice::Error, fx::ERROR_MS, &mut out);
    assert!(out.iter().all(|c| *c == Col::default()));
}

fn lit(out: &[Col]) -> Vec<usize> {
    (0..PANEL_LEDS).filter(|&i| lum(out[i]) > 0.05).collect()
}

#[test]
fn spectrum_bass_lights_the_middle_columns() {
    let st = fx::MusicState::default();
    let mut bands = [0.0f32; 8];
    bands[0] = 0.9;
    let inp = fx::MusicInput {
        bands,
        loudness: 0.5,
        flash: 0.0,
    };
    let mut out = vec![Col::default(); 88];
    music(fx::Style::Spectrum, &st, &inp, 0.0, &mut out);
    // Горят только столбики басов, в середине; полосы без звука тёмные.
    let on = lit(&out);
    assert!(!on.is_empty());
    assert!(on.iter().all(|&i| GEOMETRY[i].dx.abs() < 0.5), "{on:?}");
    let silent = fx::MusicInput {
        bands: [0.0; 8],
        loudness: 0.0,
        flash: 0.0,
    };
    music(fx::Style::Spectrum, &st, &silent, 0.0, &mut out);
    assert!(lit(&out).is_empty(), "lit in silence: {:?}", lit(&out));
}

#[test]
fn pulse_grows_with_loudness() {
    let st = fx::MusicState::default();
    let count = |loudness: f32| {
        let inp = fx::MusicInput {
            bands: [loudness; 8],
            loudness,
            flash: 0.0,
        };
        let mut out = vec![Col::default(); 88];
        music(fx::Style::Pulse, &st, &inp, 0.0, &mut out);
        lit(&out).len()
    };
    assert!(
        count(0.9) > count(0.1) + 10,
        "{} vs {}",
        count(0.9),
        count(0.1)
    );
}

#[test]
fn speaking_and_timer_ring_light_the_contour_and_move() {
    for v in [fx::Voice::Speaking, fx::Voice::TimerRing] {
        let mut a = vec![Col::default(); 88];
        let mut b = vec![Col::default(); 88];
        voice(v, 200, &mut a);
        voice(v, 900, &mut b);
        assert!(contour_leds().iter().all(|&i| lum(a[i]) > 0.1), "{v:?}");
        assert!(a != b, "{v:?} does not move");
    }
}

#[test]
fn listening_rise_joins_the_breathing_without_a_jump() {
    let mut a = vec![Col::default(); 88];
    let mut b = vec![Col::default(); 88];
    voice(fx::Voice::Listening, 449, &mut a);
    voice(fx::Voice::Listening, 451, &mut b);
    for i in 0..PANEL_LEDS {
        assert!(
            (lum(a[i]) - lum(b[i])).abs() < 0.05,
            "LED {i}: {:?} vs {:?}",
            a[i],
            b[i]
        );
    }
}

#[test]
fn disco_and_polar_follow_the_ring_by_angle() {
    // Кольцевой стиль на виртуальном кольце: светодиод 0 кольца — верх панели.
    let st = fx::MusicState::default();
    let inp = fx::MusicInput {
        bands: [0.8; 8],
        loudness: 0.8,
        flash: 0.0,
    };
    for s in [fx::Style::Disco, fx::Style::Polar] {
        let mut ring = [Col::default(); 24];
        fx::music(s, &st, &inp, 0.0, &mut ring);
        let mut out = vec![Col::default(); 88];
        music(s, &st, &inp, 0.0, &mut out);
        assert!(!lit(&out).is_empty(), "{s:?} drew nothing");
        // Верхняя середина контура берёт цвет светодиода 0 кольца.
        let top = contour_leds()
            .into_iter()
            .min_by(|&a, &b| {
                let a = GEOMETRY[a];
                let b = GEOMETRY[b];
                (a.dx.abs() + a.dy).total_cmp(&(b.dx.abs() + b.dy))
            })
            .unwrap();
        assert!(GEOMETRY[top].dy < 0.0);
        let want = lum(ring[0]);
        assert!(
            (lum(out[top]) - want).abs() <= 0.5 * want + 0.05,
            "{s:?}: top {} vs ring[0] {want}",
            lum(out[top])
        );
    }
}

#[test]
fn mute_sweep_closes_the_contour() {
    for on in [true, false] {
        let mut out = vec![Col::default(); 88];
        mute_sweep(on, 450, &mut out);
        let full = lum(fx::stock(fx::MUTE_RED, 1.0));
        for i in contour_leds() {
            assert!(lum(out[i]) > 0.99 * full, "on={on} LED {i}: {:?}", out[i]);
        }
    }
}

#[test]
fn pulse_lights_the_whole_contour_evenly_in_silence() {
    let st = fx::MusicState::default();
    let inp = fx::MusicInput {
        bands: [0.0; 8],
        loudness: 0.0,
        flash: 0.0,
    };
    let mut out = vec![Col::default(); 88];
    music(fx::Style::Pulse, &st, &inp, 0.0, &mut out);
    let lums: Vec<f32> = contour_leds().into_iter().map(|i| lum(out[i])).collect();
    let (lo, hi) = lums
        .iter()
        .fold((f32::MAX, 0.0f32), |(a, b), &v| (a.min(v), b.max(v)));
    assert!(hi > 0.0 && lo > 0.99 * hi, "{lums:?}");
}

#[test]
fn volume_lights_whole_column_pairs_at_round_levels() {
    // 7 столбцов контура: на уровнях k/7 каждый светодиод либо полный, либо тусклый фон.
    for k in 0..=7 {
        let v = k as f32 / 7.0;
        let mut out = vec![Col::default(); 88];
        volume(v, &mut out);
        let full = lum(volume_colour(v));
        for i in contour_leds() {
            let r = lum(out[i]) / full;
            assert!(r < 0.11 || r > 0.99, "v={v} LED {i}: {r}");
        }
    }
}

#[test]
fn spectrum_bass_and_treble_columns_start_together() {
    let st = fx::MusicState::default();
    let inp = fx::MusicInput {
        bands: [0.15; 8],
        loudness: 0.15,
        flash: 0.0,
    };
    let mut out = vec![Col::default(); 88];
    music(fx::Style::Spectrum, &st, &inp, 0.0, &mut out);
    // Слабый звук во всех полосах: в каждой полосе горит хотя бы один светодиод.
    for band in 0..8 {
        let any = (0..PANEL_LEDS)
            .filter(|&i| (GEOMETRY[i].dx.abs().round() as usize).min(7) == band)
            .any(|i| lum(out[i]) > 0.05);
        assert!(any, "band {band} dark");
    }
}

#[test]
fn pulse_reaches_the_corners_at_full_loudness() {
    let st = fx::MusicState::default();
    let inp = fx::MusicInput {
        bands: [1.0; 8],
        loudness: 1.0,
        flash: 0.0,
    };
    let mut out = vec![Col::default(); 88];
    music(fx::Style::Pulse, &st, &inp, 0.0, &mut out);
    let max = (0..PANEL_LEDS).map(|i| lum(out[i])).fold(0.0f32, f32::max);
    assert!((0..PANEL_LEDS).all(|i| lum(out[i]) > 0.99 * max));
}
