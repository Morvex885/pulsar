use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;

const INIT: &str = "/etc/init.d/S53bluealsa-aplay";
const PLAYER: &str = "/usr/bin/bluealsa-aplay";

#[derive(Clone)]
pub(crate) struct Stock {
    marker: Arc<Path>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ownership {
    Vacant,
    PendingRestore,
    Owned,
}

impl Stock {
    pub fn load() -> io::Result<Option<(Self, Ownership)>> {
        if std::env::var("PULSAR_BT_MANAGED").as_deref() != Ok("1") {
            return Ok(None);
        }
        let Some(path) = std::env::var_os("PULSAR_BT_OWNER").filter(|path| !path.is_empty()) else {
            return Ok(None);
        };
        let marker = Arc::from(PathBuf::from(path));
        let stock = Self { marker };
        let state = stock.ownership()?;
        Ok(Some((stock, state)))
    }

    pub async fn take(&self, flags: [bool; 2]) -> io::Result<()> {
        let stock = self.clone();
        let (exe, running) = tokio::task::spawn_blocking(move || {
            let (exe, running) = Self::player_status()?;
            stock.record(flags, running)?;
            Ok::<_, io::Error>((exe, running))
        })
        .await??;
        self.confirm_take(Self::stop_if_running(exe, running)).await
    }

    async fn confirm_take(&self, stopped: impl Future<Output = io::Result<()>>) -> io::Result<()> {
        stopped.await?;
        let pending = self.pending_marker();
        // Снимок без соседнего маркера означает подтверждённый захват.
        tokio::task::spawn_blocking(move || fs::remove_file(pending)).await?
    }

    pub async fn keep_stopped(&self) -> io::Result<()> {
        let (exe, running) = tokio::task::spawn_blocking(Self::player_status).await??;
        Self::stop_if_running(exe, running).await
    }

    fn player_status() -> io::Result<(PathBuf, bool)> {
        let init = fs::metadata(INIT)?;
        if !init.is_file() || init.permissions().mode() & 0o111 == 0 {
            return Err(io::Error::other(
                "stock player init script is not executable",
            ));
        }
        let exe = fs::canonicalize(PLAYER)?;
        let running = Self::running(&exe)?;
        Ok((exe, running))
    }

    async fn stop_if_running(exe: PathBuf, running: bool) -> io::Result<()> {
        if !running {
            return Ok(());
        }
        stop_player(INIT, async move {
            tokio::task::spawn_blocking(move || Self::running(&exe)).await?
        })
        .await
    }

    pub async fn restore(&self) -> io::Result<()> {
        let ctl = self.marker.with_file_name("ctl.sh");
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
        let stock = self.clone();
        tokio::task::spawn_blocking(move || {
            if stock.snapshot_exists()? {
                return Err(io::Error::other("stock restore left ownership snapshot"));
            }
            // Старый ctl.sh восстанавливает шестибайтный снимок, но не знает о .pending.
            let pending = stock.pending_marker();
            if private_marker(&pending, 0)?.is_some() {
                fs::remove_file(pending)?;
            }
            Ok(())
        })
        .await?
    }

    fn pending_marker(&self) -> PathBuf {
        let mut path = self.marker.as_os_str().to_owned();
        path.push(".pending");
        path.into()
    }

    fn ownership(&self) -> io::Result<Ownership> {
        let snapshot = self.snapshot_exists()?;
        if private_marker(&self.pending_marker(), 0)?.is_some() {
            Ok(Ownership::PendingRestore)
        } else if snapshot {
            Ok(Ownership::Owned)
        } else {
            Ok(Ownership::Vacant)
        }
    }

    fn snapshot_exists(&self) -> io::Result<bool> {
        let Some(mut file) = private_marker(&self.marker, 6)? else {
            return Ok(false);
        };
        let mut bytes = [0; 6];
        file.read_exact(&mut bytes)?;
        if !matches!(
            bytes,
            [b'0' | b'1', b' ', b'0' | b'1', b' ', b'0' | b'1', b'\n']
        ) || file.read(&mut [0])? != 0
        {
            return Err(io::Error::other("invalid Bluetooth owner snapshot"));
        }
        Ok(true)
    }

    fn record(&self, [pairable, discoverable]: [bool; 2], running: bool) -> io::Result<()> {
        if self.ownership()? != Ownership::Vacant {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Bluetooth owner marker exists",
            ));
        }
        // Сначала .pending, затем снимок, затем первая мутация. После сбоя между
        // этими шагами снимка либо нет, либо его можно только восстановить.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.pending_marker())?
            .set_permissions(fs::Permissions::from_mode(0o600))?;
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
        result
    }

    fn running(exe: &Path) -> io::Result<bool> {
        let owner_uid = fs::metadata("/proc/self")?.uid();
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
                && fs::metadata(&pid).is_ok_and(|meta| meta.uid() == owner_uid)
                && fs::read_link(pid.join("exe")).is_ok_and(|p| p == exe)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn private_marker(path: &Path, size: u64) -> io::Result<Option<fs::File>> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != fs::metadata("/proc/self")?.uid()
        || meta.mode() & 0o7777 != 0o600
        || meta.len() != size
    {
        return Err(io::Error::other("invalid Bluetooth owner marker"));
    }
    Ok(Some(file))
}

pub(crate) fn plugin_present() -> bool {
    let confdir = std::env::var_os("ALSA_CONFIG_DIR")
        .map_or_else(|| PathBuf::from("/usr/share/alsa"), PathBuf::from);
    let config = std::env::var_os("ALSA_CONFIG_PATH")
        .map_or_else(|| confdir.join("alsa.conf"), PathBuf::from);
    plugin_registered(&config, &confdir, Path::new("/etc/alsa"))
        && Path::new("/usr/lib/alsa-lib/libasound_module_pcm_bluealsa.so").is_file()
}

fn plugin_registered(config: &Path, confdir: &Path, etc_alsa: &Path) -> bool {
    let read_config = |path: &Path| -> io::Result<String> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        let mut text = String::new();
        file.take(128 * 1024).read_to_string(&mut text)?;
        Ok(text)
    };
    let defined = |s: &str| {
        s.lines()
            .any(|line| line.trim_start().starts_with("pcm.bluealsa"))
    };
    let registered = |path: &Path, text: &str| {
        defined(text)
            || (text.contains("conf.d")
                && [
                    path.with_file_name("conf.d").join("20-bluealsa.conf"),
                    etc_alsa.join("conf.d/20-bluealsa.conf"),
                ]
                .iter()
                .any(|path| read_config(path).is_ok_and(|s| defined(&s))))
    };
    let Ok(text) = read_config(config) else {
        return false;
    };
    // Мини: ALSA_CONFIG_PATH указывает на файл pulsar, а штатный conf.d
    // загружается из стандартной конфигурации через этот include.
    registered(config, &text)
        || (text
            .lines()
            .any(|line| line.trim() == "<confdir:alsa.conf>")
            && {
                let stock = confdir.join("alsa.conf");
                stock != config && read_config(&stock).is_ok_and(|text| registered(&stock, &text))
            })
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

    #[test]
    fn mini_config_finds_bluealsa_registration_in_stock_include() {
        let dir = test_dir("alsa-config");
        let confdir = dir.join("alsa");
        let etc_alsa = dir.join("etc-alsa");
        let config = dir.join("pulsar.conf");
        fs::create_dir_all(confdir.join("conf.d")).unwrap();
        fs::write(&config, "<confdir:alsa.conf>\npcm.mini { type plug }\n").unwrap();
        fs::write(confdir.join("alsa.conf"), "func load\nfiles [ conf.d ]\n").unwrap();
        assert!(!plugin_registered(&config, &confdir, &etc_alsa));
        fs::write(
            confdir.join("conf.d/20-bluealsa.conf"),
            "pcm.bluealsa { type plug }\n",
        )
        .unwrap();
        assert!(plugin_registered(&config, &confdir, &etc_alsa));
        fs::write(&config, "# <confdir:alsa.conf>\npcm.mini { type plug }\n").unwrap();
        assert!(!plugin_registered(&config, &confdir, &etc_alsa));
        fs::remove_dir_all(dir).unwrap();
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
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        assert!(
            stock
                .confirm_take(stop_player(init.to_str().unwrap(), async {
                    Ok(player.exists())
                }))
                .await
                .is_err()
        );
        assert!(!player.exists());
        assert!(stock.restore().await.is_err());
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        assert!(stock.snapshot_exists().unwrap());

        fs::write(dir.join("adapter"), "").unwrap();
        assert!(stock.restore().await.is_ok());
        assert!(player.exists());
        assert_eq!(stock.ownership().unwrap(), Ownership::Vacant);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn takeover_is_not_adopted_until_stop_is_confirmed() {
        let dir = test_dir("takeover");
        let stock = Stock {
            marker: Arc::from(dir.join("owner")),
        };
        let pending = stock.pending_marker();
        assert_eq!(stock.ownership().unwrap(), Ownership::Vacant);
        stock.record([false, true], false).unwrap();
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        assert_eq!(fs::read(&stock.marker).unwrap(), b"0 1 0\n");
        stock.confirm_take(async { Ok(()) }).await.unwrap();
        assert_eq!(stock.ownership().unwrap(), Ownership::Owned);

        // Отдельный маркер переживает удаление снимка только во время отката.
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
            .unwrap();
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        fs::remove_file(&stock.marker).unwrap();
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        fs::remove_file(&pending).unwrap();
        std::os::unix::fs::symlink(&stock.marker, &pending).unwrap();
        assert!(stock.ownership().is_err());
        assert!(stock.record([true; 2], false).is_err());
        assert!(!stock.marker.exists());
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
        assert_eq!(stock.ownership().unwrap(), Ownership::Owned);
        assert!(stock.record([false; 2], false).is_err());
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
