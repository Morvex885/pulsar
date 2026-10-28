//! Ambient light sensor: iio device "ltr308als01". Lux = in_intensity_input when present,
//! otherwise (in_intensity_raw + in_intensity_offset) * in_intensity_scale. Gain/integration
//! defaults are set by the firmware (/etc/light-sensor.conf) and left untouched. Polled.

use std::path::{Path, PathBuf};

use log::info;
use pulsar_core::{Timers, actors, sysfs};
use tokio::sync::mpsc;

const IIO_DIR: &str = "/sys/bus/iio/devices/";
const SENSOR_NAME: &str = "ltr308als01";

/// strtod's numeric prefix (after leading white space) of a decimal sysfs value; None when
/// there is none. Decimal only: no inf / nan / hex forms, which sysfs never gives.
fn parse_prefix(s: &str) -> Option<f64> {
    let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let b = s.as_bytes();
    let digits = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let mut i = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    let int_end = digits(i);
    let mut mantissa = int_end > i;
    i = int_end;
    if b.get(i) == Some(&b'.') {
        let frac_end = digits(i + 1);
        mantissa |= frac_end > i + 1;
        i = frac_end;
    }
    if !mantissa {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let j = i + 1 + usize::from(matches!(b.get(i + 1), Some(b'+' | b'-')));
        let exp_end = digits(j);
        if exp_end > j {
            i = exp_end;
        }
    }
    s[..i].parse().ok()
}

fn read_num(dir: &Path, attr: &str) -> Option<f64> {
    let s = sysfs::read_file(dir.join(attr).to_str()?)?;
    parse_prefix(&s)
}

fn read_lux(dir: &Path) -> Option<f32> {
    if let Some(v) = read_num(dir, "in_intensity_input") {
        return Some(v as f32);
    }
    let raw = read_num(dir, "in_intensity_raw")?;
    let scale = read_num(dir, "in_intensity_scale").unwrap_or(1.0);
    let offset = read_num(dir, "in_intensity_offset").unwrap_or(0.0);
    Some(((raw + offset) * scale).max(0.0) as f32)
}

/// The sensor's directory under `iio_dir`.
fn find(iio_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(iio_dir)
        .ok()?
        .flatten()
        .filter(|e| !e.file_name().as_encoded_bytes().starts_with(b"."))
        .map(|e| e.path())
        .find(|dir| {
            dir.join("name")
                .to_str()
                .and_then(sysfs::read_file)
                .is_some_and(|name| sysfs::trim(&name) == SENSOR_NAME)
        })
}

/// Finds the sensor and polls it every `period_ms`; the first reading is in the channel when
/// this returns. The poller stops once the receiver is dropped. Err when the sensor is missing
/// or unreadable; the caller logs it (a missing sensor is normal on some stations).
pub fn spawn_light_sensor(period_ms: u64) -> Result<mpsc::UnboundedReceiver<f32>, String> {
    spawn_in(Path::new(IIO_DIR), period_ms)
}

fn spawn_in(iio_dir: &Path, period_ms: u64) -> Result<mpsc::UnboundedReceiver<f32>, String> {
    let Some(dir) = find(iio_dir) else {
        return Err(format!("light: iio device {SENSOR_NAME} not found"));
    };
    let Some(lux) = read_lux(&dir) else {
        return Err(format!("light: cannot read {}/", dir.display()));
    };
    info!("light: {}/ ({lux:.2} lx)", dir.display());
    let (tx, rx) = mpsc::unbounded_channel();
    if let Some(lux) = read_lux(&dir) {
        let _ = tx.send(lux);
    }
    actors::spawn_actor("light", async move {
        // As the C++ loop timer: re-armed `period_ms` after each poll; period 0 polls once more.
        let mut timers = Timers::new();
        timers.set_repeating((), period_ms, period_ms);
        loop {
            tokio::select! {
                biased;
                () = timers.next() => {
                    if let Some(lux) = read_lux(&dir)
                        && tx.send(lux).is_err()
                    {
                        break;
                    }
                }
                () = tx.closed() => break,
            }
        }
    });
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "pulsar-light-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }

        fn device(&self, name: &str, attrs: &[(&str, &str)]) -> PathBuf {
            let d = self.0.join(name);
            std::fs::create_dir_all(&d).unwrap();
            for (k, v) in attrs {
                std::fs::write(d.join(k), v).unwrap();
            }
            d
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn numeric_prefix_like_strtod() {
        assert_eq!(parse_prefix("12.5\n"), Some(12.5));
        assert_eq!(parse_prefix("  -3"), Some(-3.0));
        assert_eq!(parse_prefix(".5x"), Some(0.5));
        assert_eq!(parse_prefix("7."), Some(7.0));
        assert_eq!(parse_prefix("1e3 lx"), Some(1000.0));
        assert_eq!(parse_prefix("2e"), Some(2.0));
        assert_eq!(parse_prefix("0.012207031"), Some(0.012207031));
        assert_eq!(parse_prefix(""), None);
        assert_eq!(parse_prefix("\n"), None);
        assert_eq!(parse_prefix("-."), None);
        assert_eq!(parse_prefix("abc"), None);
    }

    #[test]
    fn lux_from_input_or_raw_offset_scale() {
        let t = TempDir::new();
        let d = t.device("a", &[("in_intensity_input", "42.25\n")]);
        assert_eq!(read_lux(&d), Some(42.25));
        let d = t.device(
            "b",
            &[
                ("in_intensity_raw", "100\n"),
                ("in_intensity_offset", "-20\n"),
                ("in_intensity_scale", "0.5\n"),
            ],
        );
        assert_eq!(read_lux(&d), Some(40.0));
        let d = t.device("c", &[("in_intensity_raw", "7\n")]);
        assert_eq!(read_lux(&d), Some(7.0));
        let d = t.device(
            "d",
            &[("in_intensity_raw", "1\n"), ("in_intensity_offset", "-5\n")],
        );
        assert_eq!(read_lux(&d), Some(0.0));
        // An unparsable input falls back to raw.
        let d = t.device(
            "e",
            &[("in_intensity_input", "\n"), ("in_intensity_raw", "3\n")],
        );
        assert_eq!(read_lux(&d), Some(3.0));
        let d = t.device("f", &[("name", "x\n")]);
        assert_eq!(read_lux(&d), None);
    }

    #[test]
    fn missing_or_unreadable_sensor_is_an_error() {
        let t = TempDir::new();
        t.device("iio:device0", &[("name", "other\n")]);
        assert_eq!(
            spawn_in(&t.0, 1000).unwrap_err(),
            "light: iio device ltr308als01 not found"
        );
        let d = t.device("iio:device1", &[("name", "ltr308als01\n")]);
        assert_eq!(
            spawn_in(&t.0, 1000).unwrap_err(),
            format!("light: cannot read {}/", d.display())
        );
        assert!(spawn_in(&t.0.join("nope"), 1000).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn first_value_at_once_then_polled_until_the_receiver_is_dropped() {
        let t = TempDir::new();
        t.device("iio:device0", &[("name", "other\n")]);
        let d = t.device(
            "iio:device1",
            &[("name", "ltr308als01\n"), ("in_intensity_input", "5\n")],
        );
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let mut rx = spawn_in(&t.0, 2000).unwrap();
                assert_eq!(rx.try_recv(), Ok(5.0));
                std::fs::write(d.join("in_intensity_input"), "9.5\n").unwrap();
                let start = tokio::time::Instant::now();
                assert_eq!(rx.recv().await, Some(9.5));
                assert_eq!(start.elapsed(), Duration::from_millis(2000));
                assert_eq!(rx.recv().await, Some(9.5));
                assert_eq!(start.elapsed(), Duration::from_millis(4000));
                drop(rx);
                assert!(actors::join_all(Duration::from_secs(1)).await);
            })
            .await;
    }
}
