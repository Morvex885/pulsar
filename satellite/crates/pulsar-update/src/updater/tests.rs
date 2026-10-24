use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::test_server::{End, Reply, Server, data, ok, raw, redirect};

/// Scratch directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pulsar-update-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp dir");
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .expect("read dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A release "binary": answers `--version` like pulsar.
fn script(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho 'pulsar {version}'\n").into_bytes()
}

/// The same with 300 kB of comments after an exit, so that it arrives over many reads.
fn big_script(version: &str) -> Vec<u8> {
    let mut s = format!("#!/bin/sh\necho 'pulsar {version}'\nexit 0\n").into_bytes();
    for _ in 0..3000 {
        s.extend_from_slice(format!("# {}\n", "x".repeat(97)).as_bytes());
    }
    s
}

fn spawn_test(server: &Server) -> (UpdaterHandle, UnboundedReceiver<UpdateEvent>) {
    spawn_with(Config {
        repo: "owner/repo".into(),
        user_agent: "User-Agent: pulsar/v1.0.0".into(),
        ca_bundle: data("ca.pem"),
        origin: server.url("/"),
    })
}

async fn local<F: Future>(f: F) -> F::Output {
    tokio::task::LocalSet::new().run_until(f).await
}

#[test]
fn tags_and_sums() {
    assert!(safe_tag("v1.2.3"));
    assert!(safe_tag("v1.2.3-rc.1+build_7"));
    assert!(safe_tag(&"v".repeat(64)));
    assert!(!safe_tag(&"v".repeat(65)));
    assert!(!safe_tag(""));
    assert!(!safe_tag("v1/2"));
    assert!(!safe_tag("v1.2.3?x"));
    assert!(!safe_tag("v1 2"));
    assert!(!safe_tag("vé"));

    let a = "a".repeat(64);
    let b = "b".repeat(64);
    let sums = format!(
        "{a}  pulsar-armhf.bin\r\n{b} *pulsar-aarch64.bin\n{}  short.bin\n{a}  zigbee router.ybl\n{b}  last.bin",
        &a[..63]
    );
    let sums = sums.as_bytes();
    assert_eq!(expected_sha(sums, "pulsar-armhf.bin"), Some(a.as_bytes()));
    assert_eq!(expected_sha(sums, "pulsar-aarch64.bin"), Some(b.as_bytes()));
    assert_eq!(expected_sha(sums, "zigbee router.ybl"), Some(a.as_bytes()));
    assert_eq!(expected_sha(sums, "last.bin"), Some(b.as_bytes()));
    assert_eq!(expected_sha(sums, "short.bin"), None);
    assert_eq!(expected_sha(sums, "pulsar-host.bin"), None);
    assert_eq!(expected_sha(sums, "*pulsar-aarch64.bin"), None);
    assert_eq!(expected_sha(format!("{a} ").as_bytes(), ""), None);
    assert_eq!(expected_sha(b"", "x"), None);
    assert_eq!(arch(), "host");
    assert_eq!(
        binary_dir(Path::new("/data/pulsar/pulsar")),
        Path::new("/data/pulsar")
    );
    assert_eq!(binary_dir(Path::new("pulsar")), Path::new("."));
    assert_eq!(binary_dir(Path::new("/pulsar")), Path::new("/"));
}

#[tokio::test]
async fn check_finds_the_latest_release() {
    local(async {
        let s = Server::start().await;
        let (up, mut events) = spawn_test(&s);
        s.route(
            "/owner/repo/releases/latest",
            redirect(&s.url("/owner/repo/releases/tag/v1.2.3")),
        );
        assert!(!up.busy());
        assert_eq!(up.check(), Some(1));
        assert!(up.busy());
        // Busy: refused without an event.
        assert_eq!(up.check(), None);
        assert_eq!(
            up.install(release(&s, "v1"), PathBuf::from("/nonexistent/pulsar")),
            None
        );
        let Some(UpdateEvent::Checked { id, result }) = events.recv().await else {
            panic!("no Checked");
        };
        assert!(!up.busy());
        let base = s.url("/owner/repo/releases/");
        assert_eq!(id, 1);
        assert_eq!(
            result,
            Ok(Release {
                version: "v1.2.3".into(),
                url: format!("{base}tag/v1.2.3"),
                bin_url: format!("{base}download/v1.2.3/pulsar-host.bin"),
                sums_url: format!("{base}download/v1.2.3/SHA256SUMS"),
            })
        );
        let head = s.requests.borrow()[0].clone();
        assert!(
            head.starts_with("GET /owner/repo/releases/latest HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("\r\nUser-Agent: pulsar/v1.0.0\r\n"), "{head}");

        let mut check = async |reply: Reply| {
            s.route("/owner/repo/releases/latest", reply);
            let id = up.check().expect("idle");
            match events.recv().await {
                Some(UpdateEvent::Checked { id: got, result }) if got == id => result,
                other => panic!("{other:?}"),
            }
        };
        // The redirect is not followed (max_redirects 0).
        assert_eq!(
            check(redirect(&s.url("/owner/repo/releases"))).await,
            Err("no release published yet".into())
        );
        let odd = s.url("/owner/repo/releases/tag/v1%202");
        assert_eq!(
            check(redirect(&odd)).await,
            Err(format!("unexpected release tag in {odd}"))
        );
        assert_eq!(check(ok(b"page")).await, Err("github.com: HTTP 200".into()));
        assert_eq!(
            check(raw("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")).await,
            Err("github.com: HTTP 404".into())
        );
        assert_eq!(
            check(raw("garbage\r\n\r\n")).await,
            Err("bad status line".into())
        );
        // Ids count on across requests.
        assert_eq!(up.check(), Some(7));
    })
    .await;
}

fn release(s: &Server, version: &str) -> Release {
    Release {
        version: version.into(),
        url: String::new(),
        bin_url: s.url(&format!("/dl/{version}/pulsar-host.bin")),
        sums_url: s.url(&format!("/dl/{version}/SHA256SUMS")),
    }
}

struct Installed {
    result: Result<(), (String, bool)>,
    progress: Vec<f32>,
}

async fn install(
    up: &UpdaterHandle,
    events: &mut UnboundedReceiver<UpdateEvent>,
    r: Release,
    binary: &Path,
) -> Installed {
    let id = up.install(r, binary.to_path_buf()).expect("idle");
    let mut progress = Vec::new();
    loop {
        match events.recv().await {
            Some(UpdateEvent::Progress { id: got, value }) if got == id => progress.push(value),
            Some(UpdateEvent::Installed { id: got, result }) if got == id => {
                assert!(!up.busy());
                return Installed { result, progress };
            }
            other => panic!("{other:?}"),
        }
    }
}

/// The install tests exec freshly written files: a fork by a parallel test while one of them is
/// still open for writing would fail the exec with ETXTBSY.
static EXEC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn install_swaps_binary_and_extras() {
    let _exec = EXEC.lock().await;
    local(async {
        let s = Server::start().await;
        let dir = TempDir::new();
        let binary = dir.join("pulsar");
        fs::write(&binary, script("v1.0.0")).expect("binary");
        let new = big_script("v9.9.9");
        let router = b"router image".to_vec();
        s.route("/dl/v9.9.9/pulsar-host.bin", ok(&new));
        // The assets of a release redirect to another host on GitHub.
        s.route("/dl/v9.9.9/zigbee-router.ybl", redirect("/cdn/router"));
        s.route("/cdn/router", ok(&router));
        s.route(
            "/dl/v9.9.9/SHA256SUMS",
            ok(format!(
                "{}  pulsar-host.bin\n{}  zigbee-router.ybl\n",
                sha256_hex(&new),
                sha256_hex(&router)
            )
            .as_bytes()),
        );
        let (up, mut events) = spawn_test(&s);
        up.add_extra_asset("zigbee-router.ybl", dir.join("zigbee-router.ybl"));
        // Not in this release (404): installed without it.
        up.add_extra_asset("zigbee-coordinator.ybl", dir.join("zigbee-coordinator.ybl"));
        let done = install(&up, &mut events, release(&s, "v9.9.9"), &binary).await;
        assert_eq!(done.result, Ok(()));
        // got * 95 / total per read, each value once.
        assert!(done.progress.len() > 3, "{:?}", done.progress);
        assert_eq!(done.progress.last(), Some(&95.0));
        assert!(done.progress.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(fs::read(&binary).expect("binary"), new);
        assert_eq!(
            fs::read(dir.join("pulsar.prev")).expect(".prev"),
            script("v1.0.0")
        );
        assert_eq!(
            fs::read(dir.join("zigbee-router.ybl")).expect("extra"),
            router
        );
        let mode = |name: &str| {
            fs::metadata(dir.join(name))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(
            (
                mode("pulsar"),
                mode("pulsar.prev"),
                mode("zigbee-router.ybl")
            ),
            (0o755, 0o755, 0o644)
        );
        assert_eq!(dir.names(), ["pulsar", "pulsar.prev", "zigbee-router.ybl"]);
        let heads = s.requests.borrow();
        assert!(
            heads
                .iter()
                .all(|h| h.contains("\r\nUser-Agent: pulsar/v1.0.0\r\n"))
        );
    })
    .await;
}

#[tokio::test]
async fn install_refuses_bad_releases() {
    let _exec = EXEC.lock().await;
    local(async {
        let s = Server::start().await;
        let dir = TempDir::new();
        let binary = dir.join("pulsar");
        let old = script("v1.0.0");
        fs::write(&binary, &old).expect("binary");
        let (up, mut events) = spawn_test(&s);
        up.add_extra_asset("zigbee-router.ybl", dir.join("zigbee-router.ybl"));
        let mut attempt = async |bin: Option<&[u8]>, sums: Option<String>, extra: Option<Reply>| {
            let v = "v9.9.9";
            let routes = [
                ("pulsar-host.bin", bin.map(ok)),
                ("SHA256SUMS", sums.map(|t| ok(t.as_bytes()))),
                ("zigbee-router.ybl", extra),
            ];
            for (name, reply) in routes {
                let reply = reply
                    .unwrap_or_else(|| raw("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"));
                s.route(&format!("/dl/{v}/{name}"), reply);
            }
            let done = install(&up, &mut events, release(&s, v), &binary).await;
            // Nothing changed, nothing left behind.
            assert_eq!(fs::read(&binary).expect("binary"), old);
            assert_eq!(dir.names(), ["pulsar"]);
            done.result
        };
        let good = script("v9.9.9");
        let sum = |data: &[u8], name: &str| format!("{}  {name}\n", sha256_hex(data));
        let fatal = |e: &str| Err((e.to_owned(), true));

        assert_eq!(
            attempt(None, None, None).await,
            fatal("pulsar-host.bin: HTTP 404")
        );
        assert_eq!(
            attempt(Some(&good), None, None).await,
            fatal("SHA256SUMS: HTTP 404")
        );
        assert_eq!(
            attempt(Some(&good), Some(sum(&good, "pulsar-armhf.bin")), None).await,
            fatal("SHA256SUMS lists no pulsar-host.bin")
        );
        assert_eq!(
            attempt(Some(&good), Some(sum(b"other", "pulsar-host.bin")), None).await,
            fatal("pulsar-host.bin: checksum mismatch")
        );
        let wrong = script("v1.0.0");
        assert_eq!(
            attempt(Some(&wrong), Some(sum(&wrong, "pulsar-host.bin")), None).await,
            fatal("pulsar-host.bin does not run here as v9.9.9 (reports 'pulsar v1.0.0')")
        );
        let silent = b"#!/bin/sh\nexit 3\n".to_vec();
        assert_eq!(
            attempt(Some(&silent), Some(sum(&silent, "pulsar-host.bin")), None).await,
            fatal("pulsar-host.bin does not run here as v9.9.9")
        );
        let sums = sum(&good, "pulsar-host.bin") + &sum(b"router", "zigbee-router.ybl");
        assert_eq!(
            attempt(Some(&good), Some(sums.clone()), Some(ok(b"tampered"))).await,
            fatal("zigbee-router.ybl: checksum mismatch")
        );
        assert_eq!(
            attempt(
                Some(&good),
                Some(sum(&good, "pulsar-host.bin")),
                Some(ok(b"router"))
            )
            .await,
            fatal("SHA256SUMS lists no zigbee-router.ybl")
        );
        // Not the release's fault: worth retrying.
        assert_eq!(
            attempt(
                Some(&good),
                Some(sums),
                Some(raw("HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n"))
            )
            .await,
            Err(("zigbee-router.ybl: HTTP 503".into(), false))
        );
    })
    .await;
}

#[tokio::test]
async fn install_reports_file_errors() {
    let _exec = EXEC.lock().await;
    local(async {
        let s = Server::start().await;
        let dir = TempDir::new();
        let good = script("v9.9.9");
        s.route("/dl/v9.9.9/pulsar-host.bin", ok(&good));
        s.route(
            "/dl/v9.9.9/SHA256SUMS",
            ok(format!("{}  pulsar-host.bin\n", sha256_hex(&good)).as_bytes()),
        );
        let (up, mut events) = spawn_test(&s);
        let missing = dir.join("nodir").join("pulsar");
        let done = install(&up, &mut events, release(&s, "v9.9.9"), &missing).await;
        let staged = dir.join("nodir").join("pulsar.new");
        assert_eq!(
            done.result,
            Err((
                format!(
                    "cannot write {}: No such file or directory",
                    staged.display()
                ),
                false
            ))
        );
        // The running binary is gone: no .prev, nothing replaced.
        let binary = dir.join("pulsar");
        let done = install(&up, &mut events, release(&s, "v9.9.9"), &binary).await;
        assert_eq!(
            done.result,
            Err((
                "cannot keep the current binary as .prev: No such file or directory".into(),
                false
            ))
        );
        assert_eq!(dir.names(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn dropping_the_handles_stops_a_download() {
    local(async {
        let s = Server::start().await;
        s.route(
            "/dl/v9.9.9/pulsar-host.bin",
            Reply {
                bytes: b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial".to_vec(),
                end: End::Hang,
            },
        );
        let (up, mut events) = spawn_test(&s);
        let id = up.install(release(&s, "v9.9.9"), PathBuf::from("/nonexistent/pulsar"));
        assert_eq!(id, Some(1));
        assert!(matches!(
            events.recv().await,
            Some(UpdateEvent::Progress { .. })
        ));
        drop(up);
        let start = Instant::now();
        assert!(pulsar_core::actors::join_all(Duration::from_secs(3)).await);
        assert!(start.elapsed() < Duration::from_secs(1));
        // The cancelled install reports nothing.
        assert!(events.recv().await.is_none());
    })
    .await;
}

/// Starts an install of `bin`, drops the handles once `ready` holds, then checks that the
/// install stopped without touching anything: no pulsar.new, no .prev, the binary as before, no
/// result event.
async fn cancel_after_download(
    bin: Vec<u8>,
    extra: Option<Reply>,
    ready: impl Fn(&Server, &TempDir) -> bool,
) {
    let s = Server::start().await;
    let dir = TempDir::new();
    let binary = dir.join("pulsar");
    let old = script("v1.0.0");
    fs::write(&binary, &old).expect("binary");
    s.route("/dl/v9.9.9/pulsar-host.bin", ok(&bin));
    let mut sums = format!("{}  pulsar-host.bin\n", sha256_hex(&bin));
    let (up, mut events) = spawn_test(&s);
    if let Some(extra) = extra {
        sums += &format!("{}  zigbee-router.ybl\n", sha256_hex(b"router"));
        s.route("/dl/v9.9.9/zigbee-router.ybl", extra);
        up.add_extra_asset("zigbee-router.ybl", dir.join("zigbee-router.ybl"));
    }
    s.route("/dl/v9.9.9/SHA256SUMS", ok(sums.as_bytes()));
    assert_eq!(up.install(release(&s, "v9.9.9"), binary.clone()), Some(1));
    let start = Instant::now();
    while !ready(&s, &dir) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            dir.names()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(up);
    let cancelled = Instant::now();
    assert!(pulsar_core::actors::join_all(Duration::from_secs(3)).await);
    assert!(cancelled.elapsed() < Duration::from_secs(1));
    while let Some(ev) = events.recv().await {
        assert!(matches!(ev, UpdateEvent::Progress { .. }), "{ev:?}");
    }
    assert_eq!(dir.names(), ["pulsar"]);
    assert_eq!(fs::read(&binary).expect("binary"), old);
}

#[tokio::test]
async fn cancel_stops_install_during_version_run() {
    let _exec = EXEC.lock().await;
    // Answers --version only after 5 s: cancelled while it runs (pulsar.new is written then).
    let slow = b"#!/bin/sh\nsleep 5\necho 'pulsar v9.9.9'\n".to_vec();
    local(cancel_after_download(slow, None, |_, d| {
        d.names().contains(&"pulsar.new".to_owned())
    }))
    .await;
}

#[tokio::test]
async fn cancel_stops_install_before_swap() {
    let _exec = EXEC.lock().await;
    // The binary passes --version; the extra after it never finishes, so the cancel comes
    // between the checks and the swap.
    let hang = Reply {
        bytes: b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nro".to_vec(),
        end: End::Hang,
    };
    local(cancel_after_download(
        script("v9.9.9"),
        Some(hang),
        |s, _| {
            s.requests
                .borrow()
                .iter()
                .any(|h| h.starts_with("GET /dl/v9.9.9/zigbee-router.ybl "))
        },
    ))
    .await;
}
