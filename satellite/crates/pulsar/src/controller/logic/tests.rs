use super::*;

#[test]
fn port_offset() {
    let base = ports(None).unwrap();
    assert_eq!(
        base,
        Ports {
            offset: 0,
            api: 6053,
            sendspin: 8928,
            deconz: 6638,
            arbiter: 6054,
        }
    );
    let test = ports(Some(OsStr::new("10000"))).unwrap();
    assert_eq!(
        (test.api, test.sendspin, test.deconz, test.arbiter),
        (16053, 18928, 16638, 16054)
    );
    assert_eq!(ports(Some(OsStr::new("0"))), Some(base));
    for bad in ["", "abc", "-1", "65536", "1e4", " 5", "60000"] {
        assert_eq!(ports(Some(OsStr::new(bad))), None, "{bad:?}");
    }
    // 8928 + 56607 = 65535 still fits, 56608 overflows the Sendspin port.
    assert!(ports(Some(OsStr::new("56607"))).is_some());
    assert_eq!(ports(Some(OsStr::new("56608"))), None);
    assert_eq!(ports(Some(OsStr::from_bytes(b"1\xff"))), None);
}

#[test]
fn state_dir_as_cxx() {
    let d = |p: &str| state_dir(OsStr::new(p));
    assert_eq!(
        d("/data/pulsar/state.conf"),
        std::path::Path::new("/data/pulsar")
    );
    assert_eq!(d("state.conf"), std::path::Path::new("."));
    assert_eq!(d("/state.conf"), std::path::Path::new(""));
    assert_eq!(
        in_dir(&d("/state.conf"), "api_key"),
        std::path::Path::new("/api_key")
    );
    assert_eq!(
        in_dir(&d("state.conf"), "sendspin"),
        std::path::Path::new("./sendspin")
    );
    assert_eq!(
        in_dir(&d("/data/pulsar/state.conf"), "zigbee-router.ybl"),
        std::path::Path::new("/data/pulsar/zigbee-router.ybl")
    );
}

#[test]
fn identity_from_mac() {
    let midi = pulsar_core::model::model_from_compatible(b"yandex,cucumber\0").unwrap();
    let lite = pulsar_core::model::model_from_compatible(b"yandex,plum\0").unwrap();
    let id = identity(midi, "aa:bb:cc:dd:Ee:0f", "v1.2.3", "Jan  1 2026, 00:00:00");
    assert_eq!(
        id,
        DeviceIdentity {
            name: "yandex-midi-ddee0f".into(),
            friendly_name: "Яндекс Станция Миди".into(),
            mac: "AA:BB:CC:DD:EE:0F".into(),
            model: "Yandex Station Midi".into(),
            board: "cucumber".into(),
            manufacturer: "Yandex".into(),
            project_version: "v1.2.3".into(),
            compilation_time: "Jan  1 2026, 00:00:00".into(),
        }
    );
    let id = identity(lite, "00:11:22:33:44:55", "unknown", "unknown");
    assert_eq!(
        (id.name.as_str(), id.board.as_str()),
        ("yandex-lite2-334455", "plum")
    );
    // No wlan0: the prefix alone, an empty MAC.
    let id = identity(lite, "", "v1", "t");
    assert_eq!((id.name.as_str(), id.mac.as_str()), ("yandex-lite2-", ""));
    assert_eq!(identity(lite, "a:b", "v1", "t").name, "yandex-lite2-ab");
}

#[test]
fn volume_migration() {
    let from_legacy = |v: f32| v * 2.0;
    // volume2= is authoritative.
    assert_eq!(
        initial_volume(true, 0.4, 0.9, false, 0.7, from_legacy),
        (0.4, false)
    );
    // volume= of an earlier build: the same raw level on the stock curve, persisted.
    assert_eq!(
        initial_volume(false, 0.5, 0.2, false, 0.7, from_legacy),
        (0.4, true)
    );
    // volume_curve=2: volume= was already on the stock curve (at most 1).
    assert_eq!(
        initial_volume(false, 0.5, 0.2, true, 0.7, from_legacy),
        (0.2, true)
    );
    assert_eq!(
        initial_volume(false, 0.5, 1.5, true, 0.7, from_legacy),
        (1.0, true)
    );
    // Neither: the mixer's volume, never above 0.3.
    assert_eq!(
        initial_volume(false, 0.5, -1.0, false, 0.7, from_legacy),
        (0.3, false)
    );
    assert_eq!(
        initial_volume(false, 0.5, -1.0, false, 0.2, from_legacy),
        (0.2, false)
    );
}

#[test]
fn led_show_colour() {
    let rgb = |r, g, b| Some(Rgb { r, g, b });
    assert_eq!(parse_led_color(""), rgb(255, 255, 255));
    assert_eq!(parse_led_color("#FF8000"), rgb(255, 128, 0));
    assert_eq!(parse_led_color("0a0B0c"), rgb(10, 11, 12));
    for bad in [
        "#", "#ff800", "#ff80001", "#gg0000", "+f8000", "##ff8000", " ff8000", "фф80",
    ] {
        assert_eq!(parse_led_color(bad), None, "{bad:?}");
    }
}

#[test]
fn timer_choice() {
    let run = |end_ms| HaTimer {
        active: true,
        end_ms,
        seconds_left: 0,
    };
    let paused = |seconds_left| HaTimer {
        active: false,
        end_ms: 0,
        seconds_left,
    };
    let now = 1_000_000;
    let mut t = BTreeMap::new();
    assert_eq!(pick_timer(&t, now), None);
    t.insert("b".to_owned(), paused(30));
    t.insert("a".to_owned(), paused(60));
    assert_eq!(pick_timer(&t, now), Some(("b", paused(30), 30_000)));
    // A running timer beats any paused one, even with more time left.
    t.insert("c".to_owned(), run(now + 90_000));
    assert_eq!(pick_timer(&t, now), Some(("c", run(now + 90_000), 90_000)));
    t.insert("d".to_owned(), run(now + 10_000));
    assert_eq!(pick_timer(&t, now), Some(("d", run(now + 10_000), 10_000)));
    // Ran out locally: skipped until HA's FINISHED removes it.
    t.insert("0".to_owned(), run(now));
    assert_eq!(pick_timer(&t, now).unwrap().0, "d");
    // Ties: the smallest id (std::map order).
    t.insert("e".to_owned(), run(now + 10_000));
    assert_eq!(pick_timer(&t, now).unwrap().0, "d");
    t.insert("1".to_owned(), run(now + 10_000));
    assert_eq!(pick_timer(&t, now).unwrap().0, "1");
    // Paused with nothing left counts as run out too.
    let mut p = BTreeMap::new();
    p.insert("x".to_owned(), paused(0));
    assert_eq!(pick_timer(&p, now), None);
}

#[test]
fn command_words() {
    use VoiceCommand::*;
    for (w, c) in [
        ("стоп", Stop),
        ("хватит", Stop),
        ("пауза", Pause),
        ("продолжи", Resume),
        ("играй", Resume),
        ("громче", Louder),
        ("тише", Quieter),
        ("дальше", Next),
        ("следующий", Next),
        ("назад", Prev),
    ] {
        assert_eq!(command_of(w), Some(c), "{w}");
    }
    for w in ["", "включи свет", "вверх", "Стоп", "стоп "] {
        assert_eq!(command_of(w), None, "{w:?}");
    }
}

#[test]
fn command_gating() {
    // The window after the wake word: 2.5 s of LISTENING / PROCESSING.
    assert!(after_wake(Phase::Listening, 12_499, 10_000));
    assert!(!after_wake(Phase::Listening, 12_500, 10_000));
    assert!(after_wake(Phase::Processing, 12_499, 10_000));
    assert!(!after_wake(Phase::Processing, 12_500, 10_000));
    assert!(!after_wake(Phase::Idle, 10_001, 10_000));
    assert!(!after_wake(Phase::Speaking, 10_001, 10_000));
}

#[test]
fn music_ownership() {
    use PlayState as P;
    let now = 10 * PAUSED_MUSIC_MS;
    let own = |m, paused, ss, conn, ended| owns_music(now, m, paused, ss, conn, ended);
    assert!(own(P::Playing, None, false, false, None));
    assert!(own(P::Loading, None, false, false, None));
    assert!(own(P::Idle, None, true, true, None));
    assert!(!own(P::Idle, None, false, true, None));
    // Paused / ended music stays this station's for PAUSED_MUSIC_MS.
    assert!(own(
        P::Paused,
        Some(now - PAUSED_MUSIC_MS + 1),
        false,
        false,
        None
    ));
    assert!(!own(
        P::Paused,
        Some(now - PAUSED_MUSIC_MS),
        false,
        false,
        None
    ));
    assert!(!own(P::Paused, None, false, false, None));
    assert!(own(
        P::Idle,
        None,
        false,
        true,
        Some(now - PAUSED_MUSIC_MS + 1)
    ));
    assert!(!own(
        P::Idle,
        None,
        false,
        true,
        Some(now - PAUSED_MUSIC_MS)
    ));
    // An ended Sendspin stream counts only while its server is connected.
    assert!(!own(P::Idle, None, false, false, Some(now - 1000)));
    // A pause time does not make a stopped player's music its own.
    assert!(!own(P::Idle, Some(now - 1000), false, false, None));
}

#[test]
fn command_permission() {
    use VoiceCommand::*;
    for c in [Stop, Pause, Resume, Louder, Quieter, Next, Prev] {
        assert!(
            command_allowed(c, true, false, false, false),
            "{c:?} after the wake word"
        );
        assert!(
            command_allowed(c, false, false, false, true),
            "{c:?} with own music"
        );
        // Silent, no music of its own: only "стоп".
        assert_eq!(
            command_allowed(c, false, false, false, false),
            c == Stop,
            "{c:?} silent"
        );
        // Answer / announcement / ring, no music: "стоп" and volume.
        assert_eq!(
            command_allowed(c, false, false, true, false),
            matches!(c, Stop | Louder | Quieter),
            "{c:?} sounding"
        );
        // A dialog past the window: only "стоп", whatever sounds or plays.
        assert_eq!(
            command_allowed(c, false, true, true, true),
            c == Stop,
            "{c:?} in a dialog"
        );
        assert_eq!(
            command_allowed(c, false, true, false, false),
            c == Stop,
            "{c:?} in a silent dialog"
        );
    }
}

#[test]
fn media_player_state() {
    use MediaState as M;
    use PlayState as P;
    for (announcing, music, ss, want) in [
        (true, P::Playing, true, M::Announcing),
        (true, P::Idle, false, M::Announcing),
        (false, P::Playing, false, M::Playing),
        (false, P::Loading, false, M::Playing),
        (false, P::Paused, false, M::Paused),
        (false, P::Paused, true, M::Playing),
        (false, P::Idle, false, M::Idle),
        (false, P::Idle, true, M::Playing),
        (false, P::Error, false, M::Idle),
        (false, P::Error, true, M::Playing),
    ] {
        assert_eq!(
            media_state(announcing, music, ss),
            want,
            "{announcing} {music:?} {ss}"
        );
    }
}

#[test]
fn ducking() {
    assert!(!want_duck(Phase::Idle, false, false, false));
    for phase in [Phase::Listening, Phase::Processing, Phase::Speaking] {
        assert!(want_duck(phase, false, false, false));
    }
    assert!(want_duck(Phase::Idle, true, false, false));
    assert!(want_duck(Phase::Idle, false, true, false));
    assert!(want_duck(Phase::Idle, false, false, true));
}

#[test]
fn volume_click_gating() {
    let click =
        |before, after, phase| want_volume_click(before, after, phase, false, false, false, false);
    let step = ((0.15, false), (0.1, false));
    // Outside the microphone's STT window and with ANNOUNCE free.
    for phase in [Phase::Idle, Phase::Processing] {
        assert!(click(step.0, step.1, phase), "{phase:?}");
    }
    // LISTENING streams to STT (and plays the wake earcon), SPEAKING plays the answer.
    for phase in [Phase::Listening, Phase::Speaking] {
        assert!(!click(step.0, step.1, phase), "{phase:?}");
    }
    // A held key at the top or bottom; f32 5 % steps down end at 7.45e-9 before 0.
    assert!(!click((1.0, false), (1.0, false), Phase::Idle));
    assert!(!click((7.45e-9, false), (0.0, false), Phase::Idle));
    // A key at 0 while muted unmutes: that is a change.
    assert!(click((0.0, true), (0.0, false), Phase::Idle));
    // Arbitrating, announcement, timer ring, closing earcon.
    let (b, a, idle) = (step.0, step.1, Phase::Idle);
    assert!(!want_volume_click(b, a, idle, true, false, false, false));
    assert!(!want_volume_click(b, a, idle, false, true, false, false));
    assert!(!want_volume_click(b, a, idle, false, false, true, false));
    assert!(!want_volume_click(b, a, idle, false, false, false, true));
}

#[test]
fn idle_and_auto_update() {
    let idle = |phase, a, r, m, ss, z| idle_for_update(phase, a, r, m, ss, z);
    assert!(idle(
        Phase::Idle,
        false,
        false,
        PlayState::Idle,
        false,
        false
    ));
    assert!(idle(
        Phase::Idle,
        false,
        false,
        PlayState::Paused,
        false,
        false
    ));
    assert!(idle(
        Phase::Idle,
        false,
        false,
        PlayState::Loading,
        false,
        false
    ));
    assert!(!idle(
        Phase::Speaking,
        false,
        false,
        PlayState::Idle,
        false,
        false
    ));
    assert!(!idle(
        Phase::Idle,
        true,
        false,
        PlayState::Idle,
        false,
        false
    ));
    assert!(!idle(
        Phase::Idle,
        false,
        true,
        PlayState::Idle,
        false,
        false
    ));
    assert!(!idle(
        Phase::Idle,
        false,
        false,
        PlayState::Playing,
        false,
        false
    ));
    assert!(!idle(
        Phase::Idle,
        false,
        false,
        PlayState::Idle,
        true,
        false
    ));
    assert!(!idle(
        Phase::Idle,
        false,
        false,
        PlayState::Idle,
        false,
        true
    ));

    use AutoUpdate::*;
    let cur = "v1.0.0";
    assert_eq!(auto_update(true, "v1.1.0", false, cur, "", true), Install);
    assert_eq!(auto_update(true, "v1.1.0", false, cur, "", false), WaitIdle);
    assert_eq!(auto_update(false, "v1.1.0", false, cur, "", true), No);
    assert_eq!(auto_update(true, "", false, cur, "", true), No);
    assert_eq!(auto_update(true, "v1.1.0", true, cur, "", true), No);
    assert_eq!(auto_update(true, "v1.0.0", false, cur, "", true), No);
    assert_eq!(auto_update(true, "v0.9.0", false, cur, "", true), No);
    // Rolled back by supervise.sh: that tag is skipped, a newer one is not.
    assert_eq!(auto_update(true, "v1.1.0", false, cur, "v1.1.0", true), No);
    assert_eq!(
        auto_update(true, "v1.2.0", false, cur, "v1.1.0", true),
        Install
    );
    // A build whose version does not parse never updates itself.
    assert_eq!(auto_update(true, "v1.1.0", false, "unknown", "", true), No);
    assert_eq!(
        auto_update(true, "v1.1.0", false, "v1.0.0-3-gabcdef0", "", true),
        Install
    );
}

#[test]
fn deleted_binary_path() {
    let s = |p: &str| strip_deleted(OsStr::new(p)).to_str().unwrap().to_owned();
    assert_eq!(s("/data/pulsar/pulsar"), "/data/pulsar/pulsar");
    assert_eq!(s("/data/pulsar/pulsar (deleted)"), "/data/pulsar/pulsar");
    assert_eq!(s(" (deleted)"), " (deleted)");
    assert_eq!(s("x (deleted) "), "x (deleted) ");
}

#[test]
fn cxx_min_max_clamp() {
    assert_eq!(cxx_min(1.0, f32::NAN), 1.0);
    assert!(cxx_min(f32::NAN, 1.0).is_nan());
    assert_eq!(cxx_max(0.0, f32::NAN), 0.0);
    assert!(cxx_clamp(f32::NAN, 0.25, 4.0).is_nan());
    assert_eq!(cxx_clamp(9.0, 0.25, 4.0), 4.0);
    assert_eq!(cxx_clamp(0.0, 0.25, 4.0), 0.25);
    // set_volume: a NaN from HA ends at 0, as std::min(1, std::max(0, v)).
    assert_eq!(cxx_min(1.0, cxx_max(0.0, f32::NAN)), 0.0);
}

#[test]
fn lux_throttle() {
    assert!(publish_lux(0.0, -1.0, 0, 0)); // first reading
    assert!(!publish_lux(5.9, 5.0, 0, 1000)); // within 1 lx
    assert!(publish_lux(6.1, 5.0, 0, 1000));
    assert!(!publish_lux(109.0, 100.0, 0, 1000)); // within 10 %
    assert!(publish_lux(111.0, 100.0, 0, 1000));
    assert!(!publish_lux(100.0, 100.0, 0, 60_000));
    assert!(publish_lux(100.0, 100.0, 0, 60_001)); // 60 s after the last one
}

/// Event binding to the dialog's run: (action, event, accepted, binding after).
#[test]
fn run_binding_table() {
    use VaEvent::*;
    #[derive(Clone, Copy)]
    enum Step {
        Start,
        Ev(VaEvent, bool),
        Reset,
    }
    use Step::*;
    let b = |awaiting_run_start, run_active, stale_run_end| RunBinding {
        awaiting_run_start,
        run_active,
        stale_run_end,
    };
    let scenarios: &[(&str, &[(Step, RunBinding)])] = &[
        (
            "normal run",
            &[
                (Start, b(true, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
                (Ev(SttEnd, true), b(false, true, false)),
                (Ev(TtsEnd, true), b(false, true, false)),
                (Ev(RunEnd, true), b(false, false, false)),
            ],
        ),
        (
            "barge-in: the replaced run's events and RUN_END come first",
            &[
                (Start, b(true, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
                (Start, b(true, true, true)),
                (Ev(TtsEnd, false), b(true, true, true)),
                (Ev(Unknown(77), false), b(true, true, true)),
                (Ev(RunEnd, false), b(true, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
                (Ev(SttEnd, true), b(false, true, false)),
                (Ev(RunEnd, true), b(false, false, false)),
            ],
        ),
        (
            "HA ends the request without a run",
            &[
                (Start, b(true, false, false)),
                (Ev(RunEnd, true), b(false, false, false)),
            ],
        ),
        (
            "an event type newer than the build counts like any other",
            &[
                (Start, b(true, false, false)),
                (Ev(Unknown(200), false), b(true, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
                (Ev(Unknown(200), true), b(false, true, false)),
            ],
        ),
        (
            "voice link lost",
            &[
                (Start, b(true, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
                (Start, b(true, true, true)),
                (Reset, b(false, false, false)),
                (Ev(RunEnd, true), b(false, false, false)),
            ],
        ),
        (
            "events with no dialog pass (the phase check drops them)",
            &[
                (Ev(SttEnd, true), b(false, false, false)),
                (Ev(RunStart, true), b(false, true, false)),
            ],
        ),
    ];
    for (name, steps) in scenarios {
        let mut rb = RunBinding::default();
        for (i, (step, want)) in steps.iter().enumerate() {
            match *step {
                Start => rb.start(),
                Reset => rb.reset(),
                Ev(ev, accepted) => assert_eq!(rb.accept(ev), accepted, "{name}: step {i}"),
            }
            assert_eq!(rb, *want, "{name}: step {i}");
        }
    }
}

#[test]
fn microphones_mutes() {
    // (on, software mute, button mute) -> software mute
    for (on, sw, hw, want) in [
        (false, false, false, true),
        (false, true, false, true),
        (false, false, true, true),
        (false, true, true, true),
        (true, true, false, false),
        (true, false, false, false),
        // The button's mute: HA's "on" changes nothing.
        (true, true, true, true),
        (true, false, true, false),
    ] {
        assert_eq!(
            sw_mute_after_switch(on, sw, hw),
            want,
            "on={on} sw={sw} hw={hw}"
        );
    }
    // Muted from HA only: the first press adds the button's mute, the second lifts both.
    assert!(sw_mute_after_button(true, true));
    assert!(!sw_mute_after_button(true, false));
    // The button never sets the software mute.
    assert!(!sw_mute_after_button(false, true));
}
