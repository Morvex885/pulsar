// Fixtures in tests/golden/settings/ come from the C++ reference (rust-port-base): Settings::load
// and Settings::save of controller.cpp, copied verbatim into a throwaway host harness built with
// `zig c++ -std=c++20 -O2`. save_*.conf: `save` of the values below; load_*.out: `load` of
// load_*.in, then `save`.

use super::*;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/settings/");

fn golden(name: &str) -> Vec<u8> {
    std::fs::read(format!("{DIR}{name}")).unwrap()
}

/// As the harness read its arguments (strtof).
fn f(s: &str) -> f32 {
    parse_f32_prefix(s)
}

fn values() -> Settings {
    Settings {
        volume: f("0.30000001"),
        volume_legacy: f("0.123456789"),
        clock_brightness: 0.0001,
        clock_scroll_ms: 1500,
        light: LightState {
            on: true,
            brightness: f("1234567"),
            r: f("0.00001"),
            g: f("123456.7"),
            b: f("3.14159265"),
            effect: "Rainbow".into(),
        },
        led_speed: 0.25,
        zigbee_flash_failed: 0x5001_abcd,
        zigbee_role: Mode::Coordinator,
        tz: "EST5EDT,M3.2.0,M11.1.0".into(),
        update_attempt: "v1.2.3".into(),
        led_case_color: "white".into(),
        muted: true,
        mic_muted: false,
        clock_on: true,
        ..Settings::default()
    }
}

#[test]
fn save_matches_cxx_byte_for_byte() {
    assert_eq!(
        Settings::default().serialize().as_bytes(),
        golden("save_default.conf")
    );
    assert_eq!(values().serialize().as_bytes(), golden("save_values.conf"));
    let extremes = Settings {
        volume: -1.0,
        volume_legacy: -1.0,
        clock_brightness: 1e-30,
        clock_scroll_ms: 100,
        light: LightState {
            on: false,
            brightness: 1e30,
            r: 0.0,
            g: 1.0,
            b: 0.5,
            effect: String::new(),
        },
        led_speed: 4.0,
        zigbee_flash_failed: 0xffff_ffff,
        zigbee_role: Mode::Router,
        tz: String::new(),
        mic_muted: true,
        clock_on: false,
        ..Settings::default()
    };
    assert_eq!(
        extremes.serialize().as_bytes(),
        golden("save_extremes.conf")
    );
}

#[test]
fn parses_a_file_written_by_cxx() {
    let mut s = Settings::default();
    s.parse(&golden("save_values.conf"));
    // Written with 6 significant digits: the values read back are those of the text.
    let want = Settings {
        volume: 0.3,
        has_volume: true,
        volume_legacy: 0.123_457,
        light: LightState {
            brightness: 1_234_570.0,
            g: 123_457.0,
            b: f("3.14159"),
            ..values().light
        },
        ..values()
    };
    assert_eq!(s, want);
    assert_eq!(s.serialize().as_bytes(), golden("save_values.conf"));
}

#[test]
fn load_then_save_matches_cxx() {
    for (name, has_volume, legacy_on_stock) in
        [("load_odd", false, true), ("load_edge", true, false)]
    {
        let mut s = Settings::default();
        s.parse(&golden(&format!("{name}.in")));
        assert_eq!(
            s.serialize(),
            String::from_utf8(golden(&format!("{name}.out"))).unwrap(),
            "{name}"
        );
        assert_eq!(
            (s.has_volume, s.legacy_on_stock),
            (has_volume, legacy_on_stock),
            "{name}"
        );
    }
}

#[test]
fn save_is_atomic_and_load_reads_it_back() {
    let dir = std::env::temp_dir().join(format!("pulsar-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.conf");
    let mut missing = Settings::default();
    assert!(!missing.load(&path));
    values().save(&path);
    assert!(!dir.join("state.conf.tmp").exists());
    assert_eq!(std::fs::read(&path).unwrap(), golden("save_values.conf"));
    let mut s = Settings::default();
    assert!(s.load(&path));
    assert_eq!(s.update_attempt, "v1.2.3");
    // Missing directory: the temporary file cannot be created (logged), nothing written.
    values().save(&dir.join("no/state.conf"));
    assert!(!dir.join("no").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

// glibc strtoul(s, nullptr, 16) with a 64-bit unsigned long (the Midi; the host).
#[cfg(target_pointer_width = "64")]
#[test]
fn strtoul_hex_matches_libc() {
    for (s, want) in [
        ("", 0),
        ("0", 0),
        ("5001abcd", 0x5001_abcd),
        ("0x5001ABCD", 0x5001_abcd),
        ("0X1f", 0x1f),
        ("0x", 0),
        ("0xg", 0),
        ("  -1", u64::MAX),
        ("+ff", 0xff),
        ("ffffffff", 0xffff_ffff),
        ("1ffffffff", 0x1_ffff_ffff),
        ("fffffffffffffffffff", u64::MAX),
        ("12zz", 0x12),
        ("-0x10", 0u64.wrapping_sub(16)),
        ("\t7", 7),
    ] {
        assert_eq!(strtoul_hex(s), want, "{s:?}");
    }
}
