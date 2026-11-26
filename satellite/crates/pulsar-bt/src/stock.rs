use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

const INIT: &str = "/etc/init.d/S53bluealsa-aplay";
const PLAYER: &str = "/usr/bin/bluealsa-aplay";

pub(crate) struct Stock {
    marker: PathBuf,
    exe: PathBuf,
}

impl Stock {
    pub fn available() -> Option<Self> {
        if std::env::var("PULSAR_BT_MANAGED").as_deref() != Ok("1")
            || !fs::metadata(INIT).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        {
            return None;
        }
        let exe = fs::canonicalize(PLAYER).ok()?;
        let marker = std::env::var_os("PULSAR_BT_OWNER")
            .map_or_else(|| PathBuf::from("/tmp/pulsar-bt-owner"), PathBuf::from);
        Some(Self { marker, exe })
    }

    pub async fn take(&self, flags: [bool; 2]) -> io::Result<()> {
        let running = self.running()?;
        self.record(flags, running)?;
        if !running {
            return Ok(());
        }
        stop_player(INIT, || self.running()).await
    }

    fn record(&self, [pairable, discoverable]: [bool; 2], running: bool) -> io::Result<()> {
        let check = || match fs::read_to_string(&self.marker) {
            Ok(line)
                if matches!(
                    line.trim_end_matches('\n')
                        .split(' ')
                        .collect::<Vec<_>>()
                        .as_slice(),
                    ["0" | "1", "0" | "1", "0" | "1"]
                ) =>
            {
                Ok(true)
            }
            Ok(_) => Err(io::Error::other("invalid Bluetooth owner marker")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        };
        if check()? {
            return Ok(());
        }
        let mut tmp = self.marker.as_os_str().to_owned();
        tmp.push(format!(".{}", std::process::id()));
        let tmp = PathBuf::from(tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        let result = writeln!(
            file,
            "{} {} {}",
            u8::from(pairable),
            u8::from(discoverable),
            u8::from(running)
        )
        .and_then(|()| fs::hard_link(&tmp, &self.marker));
        let _ = fs::remove_file(tmp);
        match result {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && check()? => Ok(()),
            other => other,
        }
    }

    fn running(&self) -> io::Result<bool> {
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            if !entry
                .file_name()
                .as_encoded_bytes()
                .iter()
                .all(u8::is_ascii_digit)
            {
                continue;
            }
            let pid = entry.path();
            if fs::read(pid.join("comm")).is_ok_and(|v| v == b"bluealsa-aplay\n")
                && fs::read_link(pid.join("exe")).is_ok_and(|p| p == self.exe)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(crate) fn plugin_present() -> bool {
    let config = std::env::var_os("ALSA_CONFIG_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/alsa/alsa.conf"), PathBuf::from);
    let Ok(text) = fs::read_to_string(&config) else {
        return false;
    };
    let defined = |s: &str| {
        s.lines()
            .any(|line| line.trim_start().starts_with("pcm.bluealsa"))
    };
    let dir = config.parent().unwrap_or(Path::new("/usr/share/alsa"));
    let registered = defined(&text)
        || (text.contains("conf.d")
            && [
                dir.join("conf.d/20-bluealsa.conf"),
                PathBuf::from("/etc/alsa/conf.d/20-bluealsa.conf"),
            ]
            .iter()
            .any(|path| fs::read_to_string(path).is_ok_and(|s| defined(&s))));
    registered && Path::new("/usr/lib/alsa-lib/libasound_module_pcm_bluealsa.so").is_file()
}

async fn stop_player(path: &str, running: impl FnOnce() -> io::Result<bool>) -> io::Result<()> {
    let mut cmd = Command::new(path);
    cmd.arg("stop").kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(2), cmd.status())
        .await
        .map_err(|_| io::Error::other("stock player stop timed out"))??;
    if !status.success() || running()? {
        return Err(io::Error::other("stock player did not stop"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_stop_or_surviving_player_denies_takeover() {
        assert!(stop_player("/bin/false", || Ok(false)).await.is_err());
        assert!(stop_player("/bin/true", || Ok(true)).await.is_err());
        assert!(stop_player("/bin/true", || Ok(false)).await.is_ok());
    }
}
