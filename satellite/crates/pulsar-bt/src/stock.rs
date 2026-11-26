use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;

const INIT: &str = "/etc/init.d/S53bluealsa-aplay";
const PLAYER: &str = "/usr/bin/bluealsa-aplay";
const OWNER: &str = "/tmp/pulsar-bt-owner";
const CTL: &str = "/data/pulsar/ctl.sh";

pub(crate) struct Stock {
    marker: Arc<Path>,
}

impl Stock {
    pub fn load() -> io::Result<Option<(Self, bool)>> {
        if std::env::var("PULSAR_BT_MANAGED").as_deref() != Ok("1") {
            return Ok(None);
        }
        let marker = std::env::var_os("PULSAR_BT_OWNER")
            .filter(|path| !path.is_empty())
            .map_or_else(
                || Arc::from(Path::new(OWNER)),
                |path| Arc::from(PathBuf::from(path)),
            );
        let stock = Self { marker };
        let owned = stock.snapshot_exists()?;
        Ok(Some((stock, owned)))
    }

    pub async fn take(&self, flags: [bool; 2]) -> io::Result<()> {
        let marker = Arc::clone(&self.marker);
        let (exe, running) = tokio::task::spawn_blocking(move || {
            let init = fs::metadata(INIT)?;
            if !init.is_file() || init.permissions().mode() & 0o111 == 0 {
                return Err(io::Error::other(
                    "stock player init script is not executable",
                ));
            }
            let exe = fs::canonicalize(PLAYER)?;
            let running = Self::running(&exe)?;
            Self { marker }.record(flags, running)?;
            Ok::<_, io::Error>((exe, running))
        })
        .await??;
        if !running {
            return Ok(());
        }
        stop_player(INIT, async move {
            tokio::task::spawn_blocking(move || Self::running(&exe)).await?
        })
        .await
    }

    pub async fn restore(&self) -> io::Result<()> {
        let ctl = if self.marker.as_ref() == Path::new(OWNER) {
            PathBuf::from(CTL)
        } else {
            self.marker.with_file_name("ctl.sh")
        };
        let mut cmd = Command::new(ctl);
        cmd.args(["bt-restore", "--self"])
            .env("PULSAR_BT_OWNER", &*self.marker)
            .kill_on_drop(true);
        let status = tokio::time::timeout(Duration::from_secs(10), cmd.status())
            .await
            .map_err(|_| io::Error::other("stock restore timed out"))??;
        if !status.success() {
            return Err(io::Error::other("stock restore failed"));
        }
        Ok(())
    }

    fn snapshot_exists(&self) -> io::Result<bool> {
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.marker)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        let invalid = || io::Error::other("invalid Bluetooth owner marker");
        if !meta.is_file()
            || meta.uid() != fs::metadata("/proc/self")?.uid()
            || meta.mode() & 0o7777 != 0o600
            || meta.len() != 6
        {
            return Err(invalid());
        }
        let mut bytes = [0; 6];
        file.read_exact(&mut bytes)?;
        if !matches!(
            bytes,
            [b'0' | b'1', b' ', b'0' | b'1', b' ', b'0' | b'1', b'\n']
        ) || file.read(&mut [0])? != 0
        {
            return Err(invalid());
        }
        Ok(true)
    }

    fn record(&self, [pairable, discoverable]: [bool; 2], running: bool) -> io::Result<()> {
        if self.snapshot_exists()? {
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
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o600)))
        .and_then(|()| fs::hard_link(&tmp, &self.marker));
        let _ = fs::remove_file(tmp);
        match result {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && self.snapshot_exists()? => Ok(()),
            other => other,
        }
    }

    fn running(exe: &Path) -> io::Result<bool> {
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
                && fs::read_link(pid.join("exe")).is_ok_and(|p| p == exe)
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

async fn stop_player(
    path: &str,
    running: impl Future<Output = io::Result<bool>>,
) -> io::Result<()> {
    let mut cmd = Command::new(path);
    cmd.arg("stop").kill_on_drop(true);
    let status = tokio::time::timeout(Duration::from_secs(2), cmd.status())
        .await
        .map_err(|_| io::Error::other("stock player stop timed out"))??;
    if !status.success() || running.await? {
        return Err(io::Error::other("stock player did not stop"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("pulsar-bt-{name}-{}-{unique}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn failed_stop_or_surviving_player_denies_takeover() {
        assert!(
            stop_player("/bin/false", async { Ok(false) })
                .await
                .is_err()
        );
        assert!(stop_player("/bin/true", async { Ok(true) }).await.is_err());
        assert!(stop_player("/bin/true", async { Ok(false) }).await.is_ok());
    }

    #[tokio::test]
    async fn partial_stop_and_failed_restore_preserve_recovery_snapshot() {
        let dir = test_dir("restore");
        let stock = Stock {
            marker: Arc::from(dir.join("owner")),
        };
        let player = dir.join("player");
        let init = dir.join("init");
        let ctl = dir.join("ctl.sh");
        fs::write(&player, "").unwrap();
        fs::write(
            &init,
            "#!/bin/sh\n[ \"$1\" = stop ] || exit 2\nrm \"${0%/*}/player\"\nexit 1\n",
        )
        .unwrap();
        fs::write(
            &ctl,
            "#!/bin/sh\n[ \"$*\" = 'bt-restore --self' ] || exit 2\n\
             [ \"$(cat \"$PULSAR_BT_OWNER\")\" = '1 0 1' ] || exit 2\n\
             [ -f \"${0%/*}/adapter\" ] || exit 1\n\
             touch \"${0%/*}/player\" && rm \"$PULSAR_BT_OWNER\"\n",
        )
        .unwrap();
        for script in [&init, &ctl] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        stock.record([true, false], true).unwrap();
        assert!(
            stop_player(init.to_str().unwrap(), async { Ok(player.exists()) })
                .await
                .is_err()
        );
        assert!(!player.exists());
        assert!(stock.restore().await.is_err());
        assert!(stock.snapshot_exists().unwrap());

        fs::write(dir.join("adapter"), "").unwrap();
        assert!(stock.restore().await.is_ok());
        assert!(player.exists());
        assert!(!stock.snapshot_exists().unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_invalid_snapshots_without_replacing_them() {
        use std::os::unix::fs::symlink;

        let dir = test_dir("marker");
        let stock = Stock {
            marker: Arc::from(dir.join("owner")),
        };
        let snapshot = &stock.marker;
        for bad in [b"1 0 1".as_slice(), b"1 0 1\n\n", b"1 0 2\n"] {
            fs::write(snapshot, bad).unwrap();
            fs::set_permissions(snapshot, fs::Permissions::from_mode(0o600)).unwrap();
            assert!(stock.record([false; 2], false).is_err());
            assert_eq!(fs::read(snapshot).unwrap(), bad);
        }
        fs::write(snapshot, b"1 0 1\n").unwrap();
        fs::set_permissions(snapshot, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(stock.record([false; 2], false).is_err());
        fs::set_permissions(snapshot, fs::Permissions::from_mode(0o600)).unwrap();
        stock.record([false; 2], false).unwrap();
        assert_eq!(fs::read(snapshot).unwrap(), b"1 0 1\n");
        let target = dir.join("target");
        fs::rename(snapshot, &target).unwrap();
        symlink(&target, snapshot).unwrap();
        assert!(stock.record([false; 2], false).is_err());
        assert_eq!(fs::read(target).unwrap(), b"1 0 1\n");
        fs::remove_file(snapshot).unwrap();
        fs::create_dir(snapshot).unwrap();
        assert!(stock.record([false; 2], false).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
