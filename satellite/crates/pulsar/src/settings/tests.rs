// Fixtures in tests/golden/settings/ come from the C++ reference (rust-port-base): Settings::load
// and Settings::save of controller.cpp, copied verbatim into a throwaway host harness built with
// `zig c++ -std=c++20 -O2`. save_*.conf: `save` of the values below; load_*.out: `load` of
// load_*.in, then `save`. Edited by hand since: `wake_preroll` (after `clock_music`; 0 in
// load_odd.in); `clock_auto_brightness` (after `wake_preroll`).

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

/// A fresh directory under the system temp dir, removed on drop.
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> Self {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "pulsar-settings-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn save_is_atomic_and_load_reads_it_back() {
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    let mut missing = Settings::default();
    assert!(!missing.load(&path));
    assert!(values().save(&path));
    assert!(!t.0.join("state.conf.tmp").exists());
    assert_eq!(std::fs::read(&path).unwrap(), golden("save_values.conf"));
    let mut s = Settings::default();
    assert!(s.load(&path));
    assert_eq!(s.update_attempt, "v1.2.3");
    // Missing directory: the temporary file cannot be created (logged), nothing written.
    assert!(!values().save(&t.0.join("no/state.conf")));
    assert!(!t.0.join("no").exists());
}

#[test]
fn stale_tmp_is_replaced() {
    // A power cut mid-write leaves `state.conf.tmp`; the next write truncates it.
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    std::fs::write(t.0.join("state.conf.tmp"), vec![b'x'; 4096]).unwrap();
    assert!(values().save(&path));
    assert_eq!(std::fs::read(&path).unwrap(), golden("save_values.conf"));
    assert!(!t.0.join("state.conf.tmp").exists());
}

// A write that fails half-way (here ENOSPC from /dev/full behind the temporary name) must not
// be renamed over the good file.
#[cfg(target_os = "linux")]
#[test]
fn failed_write_keeps_the_old_file() {
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    let tmp = t.0.join("state.conf.tmp");
    assert!(values().save(&path));
    std::os::unix::fs::symlink("/dev/full", &tmp).unwrap();
    let mut s = values();
    s.led_speed = 2.0;
    assert!(!s.save(&path));
    assert_eq!(std::fs::read(&path).unwrap(), golden("save_values.conf"));
    assert!(std::fs::symlink_metadata(&tmp).is_err());
}

/// The file's bytes, None when absent.
fn on_disk(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

const NEVER: std::time::Duration = std::time::Duration::from_secs(3600);

#[tokio::test(start_paused = true)]
async fn changes_coalesce_into_one_write_after_the_last() {
    use tokio::time::{Duration, Instant, advance, timeout};
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    let mut sf = StateFile::new(path.clone());
    let mut st = values();
    let start = Instant::now();
    for i in 0..20 {
        st.led_speed = 0.5 + 0.1 * i as f32;
        sf.changed();
        if i < 19 {
            advance(Duration::from_millis(50)).await;
        }
    }
    // Nothing written before SAVE_DELAY_MS after the last change.
    assert!(
        timeout(Duration::from_millis(SAVE_DELAY_MS - 1), sf.due())
            .await
            .is_err()
    );
    assert_eq!(on_disk(&path), None);
    sf.due().await;
    assert_eq!((Instant::now() - start).as_millis(), 19 * 50 + 1000);
    sf.flush(&st);
    assert_eq!(on_disk(&path).unwrap(), st.serialize().as_bytes());
    assert!(st.serialize().contains("\nled_speed=2.4\n"));
    // One write: nothing else pending.
    assert!(timeout(NEVER, sf.due()).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn write_now_bypasses_and_cancels_the_debounce() {
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    let mut sf = StateFile::new(path.clone());
    let mut st = values();
    st.led_speed = 3.0;
    sf.changed();
    st.zigbee_flash_failed = 0x5101_0203;
    assert!(sf.write_now(&st));
    // At once, with the change pending before it.
    assert_eq!(on_disk(&path).unwrap(), st.serialize().as_bytes());
    // The delayed write is no longer due, and flush has nothing to write.
    assert!(tokio::time::timeout(NEVER, sf.due()).await.is_err());
    std::fs::remove_file(&path).unwrap();
    sf.flush(&st);
    assert_eq!(on_disk(&path), None);
}

#[tokio::test(start_paused = true)]
async fn flush_writes_the_latest_value_once() {
    let t = TmpDir::new();
    let path = t.0.join("state.conf");
    let mut sf = StateFile::new(path.clone());
    let mut st = values();
    // Nothing changed: nothing written.
    sf.flush(&st);
    assert_eq!(on_disk(&path), None);
    st.led_speed = 1.5;
    sf.changed();
    st.led_speed = 2.5;
    sf.changed();
    // The exit flush, before the delay ran out.
    sf.flush(&st);
    assert_eq!(on_disk(&path).unwrap(), st.serialize().as_bytes());
    assert!(tokio::time::timeout(NEVER, sf.due()).await.is_err());
    std::fs::remove_file(&path).unwrap();
    sf.flush(&st);
    assert_eq!(on_disk(&path), None);
}

#[tokio::test(start_paused = true)]
async fn failed_flush_is_retried() {
    let t = TmpDir::new();
    let dir = t.0.join("later");
    let path = dir.join("state.conf");
    let mut sf = StateFile::new(path.clone());
    let st = values();
    sf.changed();
    sf.due().await;
    sf.flush(&st); // the directory is missing: logged, still pending
    assert_eq!(on_disk(&path), None);
    std::fs::create_dir(&dir).unwrap();
    sf.flush(&st);
    assert_eq!(on_disk(&path).unwrap(), golden("save_values.conf"));
}

#[tokio::test(start_paused = true)]
async fn failed_write_now_reports_false_and_stays_pending() {
    // The install path refuses to replace the binary when update_attempt is not on disk.
    let t = TmpDir::new();
    let dir = t.0.join("later");
    let path = dir.join("state.conf");
    let mut sf = StateFile::new(path.clone());
    let st = values();
    assert!(!sf.write_now(&st)); // the directory is missing: nothing written
    assert_eq!(on_disk(&path), None);
    // No delayed write is armed; the change stays pending for the next flush.
    assert!(tokio::time::timeout(NEVER, sf.due()).await.is_err());
    std::fs::create_dir(&dir).unwrap();
    sf.flush(&st);
    assert_eq!(on_disk(&path).unwrap(), golden("save_values.conf"));
    assert!(sf.write_now(&st));
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

#[test]
fn clock_auto_brightness_is_always_written() {
    let mut s = Settings::default();
    assert!(s.clock_auto_brightness);
    assert!(s.serialize().ends_with("clock_auto_brightness=1\n"));
    s.parse(b"clock_auto_brightness=0\n");
    assert!(!s.clock_auto_brightness);
    assert!(s.serialize().ends_with("clock_auto_brightness=0\n"));
    s.parse(b"clock_auto_brightness=1\n");
    assert!(s.clock_auto_brightness);
}
