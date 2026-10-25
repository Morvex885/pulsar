//! The updater actor: release check and install, one at a time.

use std::cell::Cell;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::https::{CA_BUNDLE, Cancel, HttpsResponse, Tls, https_get, os_text, sha256_hex};
use crate::sys;

const MAX_BINARY: usize = 64 << 20;
const MAX_SMALL: usize = 1 << 20;
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
const REDIRECTS: u32 = 5;
const GITHUB: &str = "https://github.com/";

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// tag_name
    pub version: String,
    /// Release page.
    pub url: String,
    /// pulsar-<arch>.bin
    pub bin_url: String,
    /// SHA256SUMS
    pub sums_url: String,
}

#[derive(Debug)]
pub enum UpdateEvent {
    Checked {
        id: u64,
        result: Result<Release, String>,
    },
    /// Download progress of an install, 0..95 (the swap takes the rest).
    Progress { id: u64, value: f32 },
    /// fatal: the release itself is unusable (checksum, does not run here), as opposed to a
    /// network failure worth retrying.
    Installed {
        id: u64,
        result: Result<(), (String, bool)>,
    },
}

/// Asset suffix of this build.
pub fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else if cfg!(target_arch = "arm") {
        "armhf"
    } else {
        "host"
    }
}

struct Extra {
    name: String,
    path: PathBuf,
}

enum Cmd {
    Check {
        id: u64,
    },
    Install {
        id: u64,
        release: Release,
        binary: PathBuf,
    },
    AddExtra(Extra),
}

#[derive(Clone)]
pub struct UpdaterHandle {
    cmds: UnboundedSender<Cmd>,
    /// The only copy: set here when a request is accepted, cleared by the actor just before it
    /// sends that request's Checked / Installed.
    busy: Rc<Cell<bool>>,
    next_id: Rc<Cell<u64>>,
}

impl UpdaterHandle {
    pub fn busy(&self) -> bool {
        self.busy.get()
    }

    /// Latest stable release. None = busy: not accepted, no event follows.
    pub fn check(&self) -> Option<u64> {
        self.start(|id| Cmd::Check { id })
    }

    /// Replaces `binary` with release `r`. None = busy: not accepted, no event follows.
    pub fn install(&self, r: Release, binary: PathBuf) -> Option<u64> {
        self.start(|id| Cmd::Install {
            id,
            release: r,
            binary,
        })
    }

    /// Another release asset installed with the binary, e.g. the radio's firmware images: `name`
    /// from the same release, checked against SHA256SUMS, written to `path`. Releases without one
    /// are installed without it. Takes effect from the next install() on.
    pub fn add_extra_asset(&self, name: &str, path: PathBuf) {
        let _ = self.cmds.send(Cmd::AddExtra(Extra {
            name: name.into(),
            path,
        }));
    }

    fn start(&self, cmd: impl FnOnce(u64) -> Cmd) -> Option<u64> {
        if self.busy.replace(true) {
            return None;
        }
        let id = self.next_id.get();
        if self.cmds.send(cmd(id)).is_err() {
            self.busy.set(false);
            return None;
        }
        self.next_id.set(id + 1);
        Some(id)
    }
}

struct Config {
    repo: String,
    user_agent: String,
    ca_bundle: PathBuf,
    /// "https://github.com/"; another origin in tests.
    origin: String,
}

/// `repo`: "owner/name" on GitHub. Call inside the controller's LocalSet.
pub fn spawn(
    repo: String,
    current_version: String,
) -> (UpdaterHandle, UnboundedReceiver<UpdateEvent>) {
    spawn_with(Config {
        repo,
        user_agent: format!("User-Agent: pulsar/{current_version}"),
        ca_bundle: CA_BUNDLE.into(),
        origin: GITHUB.into(),
    })
}

fn spawn_with(cfg: Config) -> (UpdaterHandle, UnboundedReceiver<UpdateEvent>) {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (ev_tx, ev_rx) = unbounded_channel();
    let busy = Rc::new(Cell::new(false));
    pulsar_core::actors::spawn_actor("updater", run(cfg, cmd_rx, ev_tx, busy.clone()));
    let handle = UpdaterHandle {
        cmds: cmd_tx,
        busy,
        next_id: Rc::new(Cell::new(1)),
    };
    (handle, ev_rx)
}

type Job = Pin<Box<dyn Future<Output = UpdateEvent>>>;

async fn run(
    cfg: Config,
    mut cmds: UnboundedReceiver<Cmd>,
    events: UnboundedSender<UpdateEvent>,
    busy: Rc<Cell<bool>>,
) {
    let cfg = Rc::new(cfg);
    let cancel = Rc::new(Cancel::default());
    // Copied into each install: a later add_extra_asset() does not change a running one.
    let mut extras: Vec<Rc<Extra>> = Vec::new();
    // At most one: the handle accepts no request while busy.
    let mut job: Option<Job> = None;
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => break,
                Some(Cmd::AddExtra(x)) => extras.push(Rc::new(x)),
                Some(Cmd::Check { id }) => {
                    job = Some(Box::pin(check(cfg.clone(), cancel.clone(), id)));
                }
                Some(Cmd::Install { id, release, binary }) => {
                    let job_extras = extras.clone();
                    job = Some(Box::pin(install(
                        cfg.clone(),
                        cancel.clone(),
                        events.clone(),
                        id,
                        release,
                        binary,
                        job_extras,
                    )));
                }
            },
            ev = async {
                match job.as_mut() {
                    Some(j) => j.await,
                    None => std::future::pending().await,
                }
            } => {
                job = None;
                busy.set(false);
                let _ = events.send(ev);
            }
        }
    }
    // Every handle is gone (daemon exit): downloads stop, a swap under way completes; the
    // result has no one to go to.
    if let Some(j) = job {
        cancel.cancel();
        j.await;
    }
}

/// A finished response as the C++ messages put it: "HTTP <status>" or the transport error.
fn failure(resp: &HttpsResponse) -> String {
    if resp.status != 0 {
        format!("HTTP {}", resp.status)
    } else {
        resp.error.clone()
    }
}

async fn load_tls(cfg: &Config) -> Result<Tls, String> {
    let path = cfg.ca_bundle.clone();
    tokio::task::spawn_blocking(move || Tls::load(&path))
        .await
        .unwrap_or_else(|e| Err(e.to_string()))
}

async fn check(cfg: Rc<Config>, cancel: Rc<Cancel>, id: u64) -> UpdateEvent {
    UpdateEvent::Checked {
        id,
        result: latest_release(&cfg, &cancel).await,
    }
}

async fn latest_release(cfg: &Config, cancel: &Cancel) -> Result<Release, String> {
    const MARKER: &str = "/releases/tag/";
    let tls = load_tls(cfg).await?;
    // github.com redirects releases/latest to the newest stable release (drafts and pre-releases
    // excluded), or to the release list when there is none. Unlike api.github.com it has no
    // 60-requests-per-hour limit per public IP, which speakers behind a shared VPN/CGNAT exit hit.
    let base = format!("{}{}/releases/", cfg.origin, cfg.repo);
    let ua = [cfg.user_agent.as_str()];
    let resp = https_get(
        &tls,
        &format!("{base}latest"),
        &ua,
        MAX_SMALL,
        None,
        Some(cancel),
        0,
    )
    .await;
    if resp.status == 0 {
        return Err(resp.error);
    }
    if !(300..400).contains(&resp.status) {
        return Err(format!("github.com: HTTP {}", resp.status));
    }
    let Some(at) = resp.location.find(MARKER) else {
        return Err("no release published yet".into());
    };
    let version = &resp.location[at + MARKER.len()..];
    if !safe_tag(version) {
        return Err(format!("unexpected release tag in {}", resp.location));
    }
    Ok(Release {
        url: format!("{base}tag/{version}"),
        bin_url: format!("{base}download/{version}/pulsar-{}.bin", arch()),
        sums_url: format!("{base}download/{version}/SHA256SUMS"),
        version: version.into(),
    })
}

/// Tag names go into URLs and file contents: plain characters only.
fn safe_tag(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'+'))
}

/// The hex digest for `name` in a sha256sum listing ("<hex>  <name>" or "<hex> *<name>").
fn expected_sha<'a>(sums: &'a [u8], name: &str) -> Option<&'a [u8]> {
    for line in sums.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().position(|&b| b == b' ') != Some(64) {
            continue;
        }
        let Some(name_at) = line[64..].iter().position(|&b| b != b' ') else {
            continue;
        };
        let file = &line[64 + name_at..];
        let file = file.strip_prefix(b"*").unwrap_or(file);
        if file == name.as_bytes() {
            return Some(&line[..64]);
        }
    }
    None
}

async fn install(
    cfg: Rc<Config>,
    cancel: Rc<Cancel>,
    events: UnboundedSender<UpdateEvent>,
    id: u64,
    r: Release,
    binary: PathBuf,
    extras: Vec<Rc<Extra>>,
) -> UpdateEvent {
    let mut last = None;
    let mut progress = |got: usize, total: usize| {
        let pct = if total > 0 {
            got as u64 * 95 / total as u64
        } else {
            0
        };
        if last != Some(pct) {
            last = Some(pct);
            let _ = events.send(UpdateEvent::Progress {
                id,
                value: pct as f32,
            });
        }
    };
    let result = install_release(&cfg, &cancel, &mut progress, &r, &binary, &extras).await;
    UpdateEvent::Installed { id, result }
}

type Failure = (String, bool);

/// The directory of `binary`, where pulsar.new and pulsar.prev go. A bare file name gives "."
/// ("pulsar.new" alone would be looked up in PATH by the --version run).
fn binary_dir(binary: &Path) -> PathBuf {
    match binary.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

async fn install_release(
    cfg: &Config,
    cancel: &Cancel,
    progress: &mut dyn FnMut(usize, usize),
    r: &Release,
    binary: &Path,
    extras: &[Rc<Extra>],
) -> Result<(), Failure> {
    let asset = format!("pulsar-{}.bin", arch());
    let dir = binary_dir(binary);
    let staged = dir.join("pulsar.new");
    let ua = [cfg.user_agent.as_str()];
    let tls = load_tls(cfg)
        .await
        .map_err(|e| (format!("{asset}: {e}"), false))?;

    let bin = https_get(
        &tls,
        &r.bin_url,
        &ua,
        MAX_BINARY,
        Some(progress),
        Some(cancel),
        REDIRECTS,
    )
    .await;
    // 404: the release has no such asset; retrying later would not help.
    if bin.status != 200 {
        return Err((format!("{asset}: {}", failure(&bin)), bin.status == 404));
    }
    let sums = https_get(
        &tls,
        &r.sums_url,
        &ua,
        MAX_SMALL,
        None,
        Some(cancel),
        REDIRECTS,
    )
    .await;
    if sums.status != 200 {
        return Err((
            format!("SHA256SUMS: {}", failure(&sums)),
            sums.status == 404,
        ));
    }
    let (body, got) = hash(bin.body)
        .await
        .map_err(|e| (format!("{asset}: {}", os_text(&e)), false))?;
    match expected_sha(&sums.body, &asset) {
        None => return Err((format!("SHA256SUMS lists no {asset}"), true)),
        Some(want) if want != got.as_bytes() => {
            return Err((format!("{asset}: checksum mismatch"), true));
        }
        Some(_) => {}
    }
    // Teardown (every handle gone) starts no new step; a swap already under way completes.
    if cancel.is_set() {
        return Err(cancelled());
    }
    if let Err(e) = write_file(staged.clone(), body, 0o755).await {
        return Err((
            format!("cannot write {}: {}", staged.display(), os_text(&e)),
            false,
        ));
    }
    // Cancelled while it runs: --version is killed (kill_on_drop) and pulsar.new removed.
    let says = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        says = run_version(&staged) => Some(says),
    };
    let Some(says) = says else {
        let _ = tokio::fs::remove_file(&staged).await;
        return Err(cancelled());
    };
    if says != format!("pulsar {}", r.version) {
        let _ = tokio::fs::remove_file(&staged).await;
        let reports = if says.is_empty() {
            String::new()
        } else {
            format!(" (reports '{says}')")
        };
        return Err((
            format!("{asset} does not run here as {}{reports}", r.version),
            true,
        ));
    }

    // Extras are downloaded and checked first and replaced only together with the binary: a new
    // radio image next to the old daemon would be written with the old daemon's protocol.
    let mut extras_staged: Vec<Option<PathBuf>> = Vec::with_capacity(extras.len());
    let assets_url = &r.sums_url[..r.sums_url.rfind('/').map_or(0, |i| i + 1)];
    for x in extras {
        match stage_extra(&tls, &ua, cancel, assets_url, x, &sums.body).await {
            Ok(new) => extras_staged.push(new),
            Err(failed) => {
                let staged = staged.clone();
                let _ = blocking(move || {
                    drop_staged(&staged, &extras_staged);
                    Ok(())
                })
                .await;
                return Err(failed);
            }
        }
    }
    let cancel_set = cancel.is_set();
    let binary = binary.to_path_buf();
    let targets: Vec<PathBuf> = extras.iter().map(|x| x.path.clone()).collect();
    blocking(move || {
        if cancel_set {
            drop_staged(&staged, &extras_staged);
            return Ok(Err(cancelled().0));
        }
        Ok(swap(&binary, &dir, &staged, &targets, extras_staged))
    })
    .await
    .unwrap_or_else(|e| Err(os_text(&e)))
    .map_err(|e| (e, false))
}

fn cancelled() -> Failure {
    ("cancelled".into(), false)
}

/// Downloads, checks and writes `<path>.new` of one extra asset; None if the release has none.
async fn stage_extra(
    tls: &Tls,
    ua: &[&str],
    cancel: &Cancel,
    assets_url: &str,
    x: &Extra,
    sums: &[u8],
) -> Result<Option<PathBuf>, Failure> {
    let url = format!("{assets_url}{}", x.name);
    let extra = https_get(tls, &url, ua, MAX_BINARY, None, Some(cancel), REDIRECTS).await;
    if extra.status == 404 {
        return Ok(None); // a release without it
    }
    if extra.status != 200 {
        return Err((format!("{}: {}", x.name, failure(&extra)), false));
    }
    let (body, got) = hash(extra.body)
        .await
        .map_err(|e| (format!("{}: {}", x.name, os_text(&e)), false))?;
    match expected_sha(sums, &x.name) {
        None => return Err((format!("SHA256SUMS lists no {}", x.name), true)),
        Some(want) if want != got.as_bytes() => {
            return Err((format!("{}: checksum mismatch", x.name), true));
        }
        Some(_) => {}
    }
    let new = with_suffix(&x.path, ".new");
    match write_file(new.clone(), body, 0o644).await {
        Ok(()) => Ok(Some(new)),
        Err(e) => Err((
            format!("cannot write {}: {}", new.display(), os_text(&e)),
            false,
        )),
    }
}

/// Keeps the running binary as .prev, then renames the staged extras and binary into place.
fn swap(
    binary: &Path,
    dir: &Path,
    staged: &Path,
    targets: &[PathBuf],
    mut extras_staged: Vec<Option<PathBuf>>,
) -> Result<(), String> {
    // supervise.sh restores .prev if the new binary crash-loops.
    let tmp = dir.join("pulsar.prev.tmp");
    if let Err(e) =
        copy_file(binary, &tmp, 0o755).and_then(|()| fs::rename(&tmp, dir.join("pulsar.prev")))
    {
        // /data on the Lite 2 is small: no partial copy is left behind.
        let _ = fs::remove_file(&tmp);
        drop_staged(staged, &extras_staged);
        return Err(format!(
            "cannot keep the current binary as .prev: {}",
            os_text(&e)
        ));
    }
    for (i, target) in targets.iter().enumerate() {
        let Some(new) = &extras_staged[i] else {
            continue;
        };
        if let Err(e) = fs::rename(new, target) {
            drop_staged(staged, &extras_staged);
            return Err(format!(
                "cannot replace {}: {}",
                target.display(),
                os_text(&e)
            ));
        }
        extras_staged[i] = None;
    }
    if let Err(e) = fs::rename(staged, binary) {
        let _ = fs::remove_file(staged);
        return Err(format!(
            "cannot replace {}: {}",
            binary.display(),
            os_text(&e)
        ));
    }
    sys::sync();
    Ok(())
}

fn drop_staged(staged: &Path, extras_staged: &[Option<PathBuf>]) {
    let _ = fs::remove_file(staged);
    for s in extras_staged.iter().flatten() {
        let _ = fs::remove_file(s);
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// File work off the runtime thread.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
}

/// SHA-256 of a download off the runtime thread (tens of ms for a binary on the Lite 2).
async fn hash(body: Vec<u8>) -> io::Result<(Vec<u8>, String)> {
    blocking(move || {
        let h = sha256_hex(&body);
        Ok((body, h))
    })
    .await
}

/// Writes `data` with `mode` (also under a umask), fsync'ed; a failed write leaves no file.
async fn write_file(path: PathBuf, data: Vec<u8>, mode: u32) -> io::Result<()> {
    blocking(move || {
        let written = open_for_write(&path, mode).and_then(|mut f| {
            f.write_all(&data)?;
            f.sync_all()?;
            f.set_permissions(Permissions::from_mode(mode))
        });
        if written.is_err() {
            let _ = fs::remove_file(&path);
        }
        written
    })
    .await
}

fn open_for_write(path: &Path, mode: u32) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
}

/// Copies `src` to `dst` with `mode`, fsync'ed (in the kernel: no copy of the binary in memory).
fn copy_file(src: &Path, dst: &Path, mode: u32) -> io::Result<()> {
    let mut from = File::open(src)?;
    let mut to = open_for_write(dst, mode)?;
    io::copy(&mut from, &mut to)?;
    to.sync_all()?;
    to.set_permissions(Permissions::from_mode(mode))
}

/// First line of `<path> --version`, "" if it did not exit 0 within the timeout.
async fn run_version(path: &Path) -> String {
    let deadline = Instant::now() + VERSION_TIMEOUT;
    // std resets the signal mask and SIGPIPE in the child (the C++ daemon had to: it blocked
    // SIGTERM in every thread).
    let mut child = loop {
        let spawned = Command::new(path)
            .arg("--version")
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        match spawned {
            Ok(child) => break child,
            // The file just written may still be open for writing in a child forked meanwhile
            // by another thread (until that child's exec closes it): try again shortly.
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(_) => return String::new(),
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        return String::new();
    };
    let mut out = Vec::new();
    let read = async {
        let mut buf = [0u8; 256];
        while out.len() < 1024 {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
            }
        }
    };
    let _ = tokio::time::timeout_at(deadline, read).await;
    // EOF comes while the child is still exiting: wait for it within the same deadline.
    let ok = match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            false
        }
    };
    drop(stdout);
    if !ok {
        return String::new();
    }
    let line = out.split(|&b| b == b'\n').next().unwrap_or_default();
    String::from_utf8_lossy(line).into_owned()
}

#[cfg(test)]
mod tests;
