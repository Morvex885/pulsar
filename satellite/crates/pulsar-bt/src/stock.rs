use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout_at};

const INIT: &str = "/etc/init.d/S53bluealsa-aplay";
const PLAYER: &str = "/usr/bin/bluealsa-aplay";
const LEGACY_MARKER: &str = "/tmp/pulsar-bt-owner";
const TEST_MARKER: &str = "/tmp/pulsar-test/bt-owner";
const LOCK_PROTOCOL: &[u8] = b"pulsar-bt-owner-lock/2\n";

fn trusted_dir() -> PathBuf {
    std::env::var_os("PULSAR_DIR")
        .filter(|path| !path.is_empty())
        .map_or_else(|| PathBuf::from("/data/pulsar"), PathBuf::from)
}

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

enum Snapshot {
    Legacy,
    LockAware,
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
        stock.require_ctl(&stock.ctl_path())?;
        let state = stock.ownership()?;
        Ok(Some((stock, state)))
    }

    fn ctl_path(&self) -> PathBuf {
        if self.marker.as_ref() == Path::new(TEST_MARKER) {
            self.marker.with_file_name("ctl.sh")
        } else {
            trusted_dir().join("ctl.sh")
        }
    }

    fn require_ctl(&self, ctl: &Path) -> io::Result<()> {
        let out = std::process::Command::new(ctl)
            .args(["bt-restore", "--owner-lock"])
            .env("PULSAR_BT_OWNER", &*self.marker)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()?;
        if !out.status.success() || out.stdout != LOCK_PROTOCOL {
            return Err(io::Error::other(
                "ctl.sh does not support Bluetooth owner locking",
            ));
        }
        Ok(())
    }

    fn lock_path(&self) -> PathBuf {
        if self.marker.as_ref() == Path::new(LEGACY_MARKER) {
            trusted_dir().join(".pulsar-bt-lock")
        } else {
            self.marker.with_file_name(".pulsar-bt-lock")
        }
    }

    fn lock(&self) -> io::Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.lock_path())?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != fs::metadata("/proc/self")?.uid()
            || meta.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::other("invalid Bluetooth ownership lock"));
        }
        file.try_lock()?;
        Ok(file)
    }

    pub async fn take(
        &self,
        deadline: Instant,
        flags: impl Future<Output = Option<[bool; 2]>>,
    ) -> io::Result<bool> {
        let lock = timeout_at(deadline, async {
            loop {
                let stock = self.clone();
                match tokio::task::spawn_blocking(move || {
                    let lock = stock.lock()?;
                    stock.require_ctl(&stock.ctl_path())?;
                    Ok::<_, io::Error>(lock)
                })
                .await?
                {
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        sleep(Duration::from_millis(50)).await;
                    }
                    result => return result,
                }
            }
        })
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::WouldBlock))??;

        // Возврат мог изменить BlueZ, пока захват ждал flock.
        let Some(flags) = flags.await else {
            return Ok(false);
        };
        let stock = self.clone();
        let (lock, exe, running) = tokio::task::spawn_blocking(move || {
            let (exe, running) = Self::player_status()?;
            stock.record(flags, running)?;
            Ok::<_, io::Error>((lock, exe, running))
        })
        .await??;
        self.confirm_take(&lock, Self::stop_if_running(&lock, exe, running))
            .await?;
        Ok(true)
    }

    async fn confirm_take(
        &self,
        lock: &fs::File,
        stopped: impl Future<Output = io::Result<()>>,
    ) -> io::Result<()> {
        stopped.await?;
        let pending = self.pending_marker();
        let lock = lock.try_clone()?;
        // Снимок без соседнего маркера означает подтверждённый захват.
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            fs::remove_file(pending)
        })
        .await?
    }

    pub async fn keep_stopped(&self) -> io::Result<()> {
        let stock = self.clone();
        let (lock, exe, running) = tokio::task::spawn_blocking(move || {
            let lock = stock.lock()?;
            stock.require_ctl(&stock.ctl_path())?;
            if stock.ownership()? != Ownership::Owned {
                return Err(io::Error::other("Bluetooth ownership no longer confirmed"));
            }
            let (exe, running) = Self::player_status()?;
            Ok::<_, io::Error>((lock, exe, running))
        })
        .await??;
        Self::stop_if_running(&lock, exe, running).await
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

    async fn stop_if_running(lock: &fs::File, exe: PathBuf, running: bool) -> io::Result<()> {
        if !running {
            return Ok(());
        }
        stop_player(lock, INIT, async move {
            tokio::task::spawn_blocking(move || Self::running(&exe)).await?
        })
        .await
    }

    pub async fn restore(&self) -> io::Result<()> {
        self.restore_with(&self.ctl_path()).await
    }

    async fn restore_with(&self, ctl: &Path) -> io::Result<()> {
        let stock = self.clone();
        let ctl_for_probe = ctl.to_owned();
        tokio::task::spawn_blocking(move || stock.require_ctl(&ctl_for_probe)).await??;
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
            let _lock = stock.lock()?;
            if stock.ownership()? != Ownership::Vacant {
                return Err(io::Error::other("stock restore left ownership markers"));
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
        let snapshot = self.snapshot()?;
        if private_marker(&self.pending_marker(), &[0])?.is_some() {
            return Ok(Ownership::PendingRestore);
        }
        // Старый ctl не должен прочитать снимок, пока новый init stop ещё работает.
        Ok(match snapshot {
            Some(Snapshot::Legacy) => Ownership::PendingRestore,
            Some(Snapshot::LockAware) => Ownership::Owned,
            None => Ownership::Vacant,
        })
    }

    fn snapshot(&self) -> io::Result<Option<Snapshot>> {
        let Some(mut file) = private_marker(&self.marker, &[6, 9])? else {
            return Ok(None);
        };
        let mut bytes = [0; 9];
        file.read_exact(&mut bytes[..6])?;
        let (format, flags) = if bytes.starts_with(b"v2 ") {
            file.read_exact(&mut bytes[6..])?;
            (Snapshot::LockAware, &bytes[3..])
        } else {
            (Snapshot::Legacy, &bytes[..6])
        };
        if !matches!(
            flags,
            [b'0' | b'1', b' ', b'0' | b'1', b' ', b'0' | b'1', b'\n']
        ) || file.read(&mut [0])? != 0
        {
            return Err(io::Error::other("invalid Bluetooth owner snapshot"));
        }
        Ok(Some(format))
    }

    fn record(&self, [pairable, discoverable]: [bool; 2], running: bool) -> io::Result<()> {
        if self.marker.as_ref() == Path::new(LEGACY_MARKER) {
            return Err(io::Error::other("legacy Bluetooth owner is restore-only"));
        }
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
            "v2 {} {} {}",
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

fn private_marker(path: &Path, sizes: &[u64]) -> io::Result<Option<fs::File>> {
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
        || !sizes.contains(&meta.len())
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

fn stop_command(lock: &fs::File, path: &str) -> io::Result<std::process::Command> {
    let mut cmd = std::process::Command::new("/bin/sh");
    // Stdio переносит flock (тот же open file description) через exec.
    // Оболочка держит его до конца stop, даже если Pulsar уже завершился.
    cmd.args([
        "-c",
        "exec 8<&0 </dev/null || exit 1; \"$1\" stop; exit $?",
        "pulsar-bt-stop",
        path,
    ])
    .stdin(Stdio::from(lock.try_clone()?));
    Ok(cmd)
}

async fn stop_player(
    lock: &fs::File,
    path: &str,
    running: impl Future<Output = io::Result<bool>>,
) -> io::Result<()> {
    // Не убиваем оболочку при отмене/таймауте: stop ещё может менять систему.
    let mut cmd = Command::from(stop_command(lock, path)?);
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
        let dir = test_dir("stop-result");
        let lock = fs::File::create(dir.join("lock")).unwrap();
        lock.lock().unwrap();
        assert!(
            stop_player(&lock, "/bin/false", async { Ok(false) })
                .await
                .is_err()
        );
        assert!(
            stop_player(&lock, "/bin/true", async { Ok(true) })
                .await
                .is_err()
        );
        assert!(
            stop_player(&lock, "/bin/true", async { Ok(false) })
                .await
                .is_ok()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn contended_take_expires_without_sampling_flags_or_creating_a_snapshot() {
        let dir = test_dir("take-deadline");
        let stock = Stock {
            marker: Arc::from(dir.join("owner")),
        };
        let lock = stock.lock().unwrap();
        let result = stock
            .take(Instant::now() + Duration::from_millis(20), async {
                panic!("adapter flags must only be sampled after taking the lock")
            })
            .await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(stock.ownership().unwrap(), Ownership::Vacant);
        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn in_flight_stop_keeps_restore_locked_after_daemon_descriptors_close() {
        use std::io::{BufRead as _, BufReader};

        let dir = test_dir("stop-locked");
        let stock = Stock {
            marker: Arc::from(dir.join("owner")),
        };
        let lock = stock.lock().unwrap();
        let gate = fs::File::create(dir.join("gate")).unwrap();
        gate.lock().unwrap();
        let init = dir.join("init");
        fs::write(
            &init,
            "#!/bin/sh\n[ \"$1\" = stop ] || exit 2\nprintf 'ready\\n'\n\
             flock -x \"${0%/*}/gate\" touch \"${0%/*}/stopped\"\n",
        )
        .unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = stop_command(&lock, init.to_str().unwrap())
            .unwrap()
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        drop(lock);
        let restore_lock = fs::File::open(stock.lock_path()).unwrap();
        let blocked = restore_lock.try_lock().is_err();
        drop(gate);
        assert!(child.wait().unwrap().success());
        assert!(blocked);
        restore_lock.try_lock().unwrap();
        assert!(dir.join("stopped").exists());
        fs::remove_dir_all(dir).unwrap();
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
            "#!/bin/sh\n\
             if [ \"$*\" = 'bt-restore --owner-lock' ]; then printf 'pulsar-bt-owner-lock/2\\n'; exit; fi\n\
             [ \"$*\" = 'bt-restore --self' ] || exit 2\n\
             [ \"$(cat \"$PULSAR_BT_OWNER\")\" = 'v2 1 0 1' ] || exit 2\n\
             [ -f \"${0%/*}/adapter\" ] || exit 1\n\
             touch \"${0%/*}/player\" && rm \"$PULSAR_BT_OWNER\" \"$PULSAR_BT_OWNER.pending\"\n",
        )
        .unwrap();
        for script in [&init, &ctl] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o700)).unwrap();
        }
        stock.record([true, false], true).unwrap();
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        let lock = stock.lock().unwrap();
        assert!(
            stock
                .confirm_take(
                    &lock,
                    stop_player(&lock, init.to_str().unwrap(), async { Ok(player.exists()) })
                )
                .await
                .is_err()
        );
        drop(lock);
        assert!(!player.exists());
        assert!(stock.restore_with(&ctl).await.is_err());
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
        assert!(stock.snapshot().unwrap().is_some());

        fs::write(dir.join("adapter"), "").unwrap();
        assert!(stock.restore_with(&ctl).await.is_ok());
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
        assert_eq!(fs::read(&stock.marker).unwrap(), b"v2 0 1 0\n");
        let lock = stock.lock().unwrap();
        stock.confirm_take(&lock, async { Ok(()) }).await.unwrap();
        drop(lock);
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
        assert_eq!(stock.ownership().unwrap(), Ownership::PendingRestore);
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
