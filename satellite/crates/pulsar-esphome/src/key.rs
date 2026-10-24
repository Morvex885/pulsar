//! The Noise pre-shared key: `api_key` (base64 of 32 bytes, ESPHome's `api: encryption: key`),
//! written by Home Assistant's auto-provisioning (NoiseEncryptionSetKeyRequest) or `ctl.sh
//! api-key`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use log::{error, info, warn};

use crate::sys;

pub type Psk = [u8; 32];

/// The key in force. Without a key Noise sessions run on the all-zeros key: ESPHome's way to
/// let HA provision a key without sending it in the clear (protects against sniffing only).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Key {
    pub psk: Psk,
    pub has_key: bool,
    /// Failed closed without a random key (getrandom failed): every Noise handshake is
    /// refused as a wrong key, and plaintext as "requires encryption" (`has_key`).
    pub reject_all: bool,
}

/// Standard base64 (padding optional, anything after '=' ignored); Some only for exactly 32
/// decoded bytes.
pub fn decode_psk(input: &[u8]) -> Option<Psk> {
    let mut out = [0u8; 32];
    let mut n = 0;
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &ch in input {
        let v = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            if n == out.len() {
                return None;
            }
            out[n] = (acc >> bits) as u8;
            n += 1;
        }
    }
    (n == out.len()).then_some(out)
}

/// Strips the white space the C++ server trimmed (" \t\r\n"; not \v or \f).
pub fn trim(s: &[u8]) -> &[u8] {
    let ws = |c: &u8| b" \t\r\n".contains(c);
    let start = s.iter().position(|c| !ws(c)).unwrap_or(s.len());
    let end = s.iter().rposition(|c| !ws(c)).map_or(start, |i| i + 1);
    &s[start..end]
}

/// `io::Error` as C strerror text, without std's " (os error N)".
pub fn os_error_text(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}

/// Reads the key file. Missing / empty file or the all-zeros key: no key. A file that cannot be
/// read or is not base64 of 32 bytes fails closed: someone meant to enable encryption, so there
/// is no fallback to plaintext; a random key keeps every client out until the file is fixed
/// (or removed).
pub fn load_key(path: Option<&Path>) -> Key {
    let Some(path) = path else {
        return Key::default();
    };
    let mut buf = Vec::with_capacity(256);
    match File::open(path).and_then(|f| f.take(256).read_to_end(&mut buf)) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => {
            error!(
                "api: cannot read {}: {}; no client can connect until it is fixed",
                path.display(),
                os_error_text(&e)
            );
            return fail_closed();
        }
    }
    let s = trim(&buf);
    if s.is_empty() {
        info!("api: no encryption key ({}), plaintext API", path.display());
        return Key::default();
    }
    let Some(psk) = decode_psk(s) else {
        error!(
            "api: {} is not base64 of 32 bytes; no client can connect until it is fixed",
            path.display()
        );
        return fail_closed();
    };
    if psk == [0u8; 32] {
        return Key::default();
    }
    info!("api: Noise encryption enabled");
    Key {
        psk,
        has_key: true,
        reject_all: false,
    }
}

fn fail_closed() -> Key {
    let mut psk = [0u8; 32];
    let reject_all = !sys::getrandom(&mut psk);
    if reject_all {
        // A fixed fallback key would be a known key: refuse everyone instead.
        error!("api: getrandom failed; rejecting every client");
    }
    Key {
        psk,
        has_key: true,
        reject_all,
    }
}

/// Outcome of NoiseEncryptionSetKeyRequest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetKey {
    /// Answered with success = false.
    Rejected,
    /// The key in force was sent again: success, nothing changes.
    Unchanged,
    /// The file changed: reload it and disconnect every client.
    Changed,
}

/// Writes (or with an empty `b64` removes) the key file. `b64` is already trimmed.
pub fn set_key(path: Option<&Path>, b64: &[u8], current: &Key) -> SetKey {
    let Some(path) = path else {
        return SetKey::Rejected;
    };
    if b64.is_empty() {
        if let Err(e) = fs::remove_file(path)
            && e.kind() != io::ErrorKind::NotFound
        {
            warn!(
                "api: cannot remove {}: {}",
                path.display(),
                os_error_text(&e)
            );
            return SetKey::Rejected;
        }
        info!("api: encryption key cleared by client");
        return SetKey::Changed;
    }
    let psk = match decode_psk(b64) {
        Some(k) if k != [0u8; 32] => k,
        _ => {
            warn!("api: rejected encryption key (must be base64 of 32 non-zero bytes)");
            return SetKey::Rejected;
        }
    };
    if current.has_key && psk == current.psk {
        return SetKey::Unchanged;
    }
    let mut data = b64.to_vec();
    data.push(b'\n');
    if let Err(e) = write_file_atomic(path, &data) {
        warn!(
            "api: cannot write {}: {}",
            path.display(),
            os_error_text(&e)
        );
        return SetKey::Rejected;
    }
    info!("api: encryption key set by client");
    SetKey::Changed
}

/// Atomic replace: a power cut while HA rotates the key must leave either key, never a torn one.
fn write_file_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A leftover temporary file (power cut mid-write) must not lend its mode or owner: the key
    // goes into a file created now, 0600.
    if let Err(e) = fs::remove_file(&tmp)
        && e.kind() != io::ErrorKind::NotFound
    {
        return Err(e);
    }
    let write = || {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        // close(2) is not checked (File's drop ignores its result): fsync above has already
        // reported any write-back error, and on the local filesystems of /data close has
        // nothing left to report.
        drop(f);
        fs::rename(&tmp, path)
    };
    let r = write();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
        return r;
    }
    // The rename survives a power cut only once the directory is synced. Not a failure of the
    // request: the new key is in place and in force from now on.
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if let Err(e) = File::open(dir).and_then(|d| d.sync_all()) {
        warn!("api: cannot sync {}: {}", dir.display(), os_error_text(&e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::golden::{cases, hex, unhex};
    use std::os::unix::fs::PermissionsExt;

    const KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    fn key_bytes() -> Psk {
        std::array::from_fn(|i| i as u8)
    }

    /// A fresh directory under the target's tmp dir, removed on drop.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "pulsar-esphome-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn decode_psk_golden() {
        for c in cases("psk") {
            let got = decode_psk(&unhex(c[0])).map_or("fail".into(), |k| hex(&k));
            assert_eq!(got, c[1], "psk {}", c[0]);
        }
    }

    #[test]
    fn trim_golden() {
        for c in cases("trim") {
            assert_eq!(hex(trim(&unhex(c[0]))), c[1], "trim {}", c[0]);
        }
    }

    #[test]
    fn load_key_cases() {
        let d = TmpDir::new("load");
        // Encryption unsupported.
        assert_eq!(load_key(None), Key::default());
        // No file.
        let path = d.file("api_key");
        assert_eq!(load_key(Some(&path)), Key::default());
        // Empty, or only white space.
        fs::write(&path, " \n").unwrap();
        assert_eq!(load_key(Some(&path)), Key::default());
        // All-zeros key: no key.
        fs::write(&path, "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\n").unwrap();
        assert_eq!(load_key(Some(&path)), Key::default());
        // Valid, with the newline set_key writes.
        fs::write(&path, format!("{KEY_B64}\n")).unwrap();
        assert_eq!(
            load_key(Some(&path)),
            Key {
                psk: key_bytes(),
                has_key: true,
                reject_all: false,
            }
        );
        // Corrupt: fails closed with a random key.
        fs::write(&path, "not a key").unwrap();
        let a = load_key(Some(&path));
        let b = load_key(Some(&path));
        assert!(a.has_key && b.has_key && !a.reject_all);
        assert_ne!(a.psk, [0; 32]);
        assert_ne!(a.psk, b.psk);
        // 31 bytes are corrupt too.
        fs::write(&path, "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==").unwrap();
        assert!(load_key(Some(&path)).has_key);
        // Only the first 256 bytes count: a valid key followed by junk past them still loads.
        fs::write(&path, format!("{KEY_B64}{}", " ".repeat(300) + "x")).unwrap();
        assert_eq!(load_key(Some(&path)).psk, key_bytes());
        // Unreadable (here a directory): fails closed too, only a missing file means no key.
        let dir = d.file("api_key_dir");
        fs::create_dir(&dir).unwrap();
        let k = load_key(Some(&dir));
        assert!(k.has_key && k.psk != [0; 32]);
    }

    #[test]
    fn set_key_writes_atomically_and_clears() {
        let d = TmpDir::new("set");
        let path = d.file("api_key");
        let none = Key::default();
        assert_eq!(set_key(None, KEY_B64.as_bytes(), &none), SetKey::Rejected);
        assert_eq!(set_key(Some(&path), b"junk", &none), SetKey::Rejected);
        assert_eq!(
            set_key(
                Some(&path),
                b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                &none
            ),
            SetKey::Rejected
        );
        assert!(!path.exists());

        assert_eq!(
            set_key(Some(&path), KEY_B64.as_bytes(), &none),
            SetKey::Changed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{KEY_B64}\n"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!d.file("api_key.tmp").exists());
        let current = load_key(Some(&path));
        // Same key again: success without a change.
        assert_eq!(
            set_key(Some(&path), KEY_B64.as_bytes(), &current),
            SetKey::Unchanged
        );
        // Clearing removes the file; clearing again is still a success.
        assert_eq!(set_key(Some(&path), b"", &current), SetKey::Changed);
        assert!(!path.exists());
        assert_eq!(set_key(Some(&path), b"", &none), SetKey::Changed);

        // A leftover temporary file does not lend its mode.
        let tmp = d.file("api_key.tmp");
        fs::write(&tmp, "stale").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            set_key(Some(&path), KEY_B64.as_bytes(), &none),
            SetKey::Changed
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{KEY_B64}\n"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!tmp.exists());

        // An unwritable directory: rejected, no stray temporary file.
        let missing = d.file("no/such/dir/api_key");
        assert_eq!(
            set_key(Some(&missing), KEY_B64.as_bytes(), &none),
            SetKey::Rejected
        );
    }
}
