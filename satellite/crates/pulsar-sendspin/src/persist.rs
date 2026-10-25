//! Identity and pairing store of sendspin-cpp: one file per persistence key in the state
//! directory, holding the blob's raw bytes, plus the operator's `unpaired_access` switch. Same
//! files as the C++ daemon (sendspin_service.cpp:61-107, 699-707): a pairing made by either one
//! keeps working under the other.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use log::{error, warn};

/// File whose first non-blank character `0` turns unpaired access off (pairing required).
const UNPAIRED_ACCESS: &str = "unpaired_access";

/// `<dir>/<key>`, byte for byte as the C++ daemon built it.
fn path(dir: &Path, key: &OsStr) -> PathBuf {
    let mut p = OsString::from(dir.as_os_str());
    p.push("/");
    p.push(key);
    PathBuf::from(p)
}

/// The C library's message for an OS error, without std's " (os error N)" suffix.
fn errstr(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

/// mkdir 0700, not recursive; an existing entry counts as success.
pub(crate) fn ensure_dir(dir: &Path) -> bool {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => true,
        Err(e) => {
            error!("sendspin: cannot create {}: {}", dir.display(), errstr(&e));
            false
        }
    }
}

pub(crate) fn load_blob(dir: &Path, key: &OsStr) -> Option<Vec<u8>> {
    fs::read(path(dir, key)).ok()
}

/// Atomic and durable: `<key>.tmp` (0600) written and fsynced, renamed over `<key>`, then the
/// directory fsynced.
pub(crate) fn save_blob(dir: &Path, key: &OsStr, data: &[u8]) -> bool {
    let p = path(dir, key);
    let mut tmp = OsString::from(p.as_os_str());
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut f = match OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
    {
        Ok(f) => f,
        Err(e) => {
            warn!("sendspin: cannot write {}: {}", tmp.display(), errstr(&e));
            return false;
        }
    };
    let written = f.write_all(data).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, &p)) {
        warn!(
            "sendspin: cannot store {}: {}",
            key.to_string_lossy(),
            errstr(&e)
        );
        let _ = fs::remove_file(&tmp);
        return false;
    }
    if let Ok(d) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
    {
        let _ = d.sync_all();
    }
    true
}

/// Unpaired access lets Music Assistant use the speaker after the operator approves it there;
/// on unless `<dir>/unpaired_access` starts (after blanks) with `0`.
pub(crate) fn unpaired_access(dir: &Path) -> bool {
    fs::read(path(dir, OsStr::new(UNPAIRED_ACCESS))).map_or(true, |b| unpaired_from(&b))
}

/// C++ `f >> c` (skips C-locale white space), then `c != '0'`; nothing to read = on.
fn unpaired_from(content: &[u8]) -> bool {
    content
        .iter()
        .find(|c| !matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .is_none_or(|&c| c != b'0')
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "pulsar-sendspin-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            Self(p)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn dir_is_created_0700_once() {
        let t = TmpDir::new();
        assert!(ensure_dir(&t.0));
        assert_eq!(mode(&t.0), 0o700);
        assert!(ensure_dir(&t.0));
        // Not recursive, like mkdir(2).
        assert!(!ensure_dir(&t.0.join("a/b")));
    }

    #[test]
    fn blobs_round_trip_as_raw_files() {
        let t = TmpDir::new();
        assert!(ensure_dir(&t.0));
        let key = OsStr::new("keypair");
        assert_eq!(load_blob(&t.0, key), None);

        let blob: Vec<u8> = (0..32).collect();
        assert!(save_blob(&t.0, key, &blob));
        let file = t.0.join("keypair");
        assert_eq!(fs::read(&file).unwrap(), blob);
        assert_eq!(mode(&file), 0o600);
        assert!(!t.0.join("keypair.tmp").exists());
        assert_eq!(load_blob(&t.0, key), Some(blob));

        // Overwrite with a shorter blob; an empty one is a valid stored value.
        assert!(save_blob(&t.0, key, &[7, 8]));
        assert_eq!(load_blob(&t.0, key), Some(vec![7, 8]));
        assert!(save_blob(&t.0, OsStr::new("rec_0"), &[]));
        assert_eq!(load_blob(&t.0, OsStr::new("rec_0")), Some(vec![]));

        // A file written by the C++ daemon is read back as is.
        fs::write(t.0.join("output_delay"), [0x2c, 0x01]).unwrap();
        assert_eq!(
            load_blob(&t.0, OsStr::new("output_delay")),
            Some(vec![0x2c, 0x01])
        );
    }

    #[test]
    fn save_fails_without_the_directory() {
        let t = TmpDir::new();
        assert!(!save_blob(&t.0, OsStr::new("keypair"), &[1]));
        assert!(!t.0.exists());
    }

    #[test]
    fn unpaired_access_switch() {
        let t = TmpDir::new();
        assert!(ensure_dir(&t.0));
        assert!(unpaired_access(&t.0));
        for (content, on) in [
            (&b"0"[..], false),
            (b"0\n", false),
            (b" \t\r\n\x0b\x0c0", false),
            (b"00", false),
            (b"1", true),
            (b"off", true),
            (b"", true),
            (b" \n", true),
            (b"\x000", true),
        ] {
            fs::write(t.0.join(UNPAIRED_ACCESS), content).unwrap();
            assert_eq!(unpaired_access(&t.0), on, "{content:?}");
        }
    }
}
