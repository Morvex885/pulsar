//! Updater probe: exercises the HTTPS client and the release updater without the daemon.
//! usage: update_probe versions
//!          release_newer() table for a few describe strings (no network)
//!        update_probe get <https_url>
//!          status, size and sha256 of the body (redirects followed)
//!        update_probe check [owner/repo]
//!          latest stable release as the daemon sees it (default Morvex885/pulsar)
//!        update_probe install <version> <bin_url> <sums_url> <binary>
//!          downloads, verifies and swaps <binary> (keeps <binary dir>/pulsar.prev), as the daemon
//!          does for a release named <version>
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use pulsar_update::https::{self, CA_BUNDLE, HttpsResponse, Tls};
use pulsar_update::{Release, UpdateEvent};

const REPO: &str = pulsar_update::UPDATE_REPO;

fn main() -> ExitCode {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let args: Vec<String> = raw
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match (args.get(1).map(String::as_str), args.len()) {
        (Some("versions"), _) => versions(),
        (Some("get"), 3) => run(get(&args[2])),
        (Some("check"), 2 | 3) => run(check(args.get(2).map_or(REPO, String::as_str))),
        // The binary path as given (not necessarily UTF-8).
        (Some("install"), 6) => run(install(&args[2..5], PathBuf::from(&raw[5]))),
        _ => {
            eprintln!(
                "usage: {} versions | get <url> | check [owner/repo] | install <version> <bin_url> <sums_url> <binary>",
                args.first().map_or("update_probe", String::as_str)
            );
            ExitCode::from(2)
        }
    }
}

/// Lines to stdout; a closed pipe is not an error worth a panic here.
fn out(bytes: &[u8]) {
    let _ = std::io::stdout().write_all(bytes);
}

fn versions() -> ExitCode {
    const CASES: [(&str, &str); 8] = [
        ("v1.2.4", "v1.2.3"),
        ("v1.2.3", "v1.2.3"),
        ("v1.2.3", "v1.2.3-4-gabc1234"),
        ("v1.3.0", "v1.2.3-4-gabc"),
        ("v1.2.3", "v1.10.0"),
        ("v1.2.3", "6ac30be"),
        ("v2.0.0", "v1.9.9-dirty"),
        ("junk", "v1.0.0"),
    ];
    for (latest, current) in CASES {
        let (newer, known) = pulsar_update::release_newer(latest, current);
        out(format!(
            "latest {latest:<8} current {current:<20} newer={} current_known={}\n",
            u8::from(newer),
            u8::from(known)
        )
        .as_bytes());
    }
    ExitCode::SUCCESS
}

/// The updater is an actor of a current-thread runtime's LocalSet, as in the daemon.
fn run(fut: impl Future<Output = ExitCode>) -> ExitCode {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::task::LocalSet::new().block_on(&rt, fut)
}

async fn get(url: &str) -> ExitCode {
    let r = match Tls::load(Path::new(CA_BUNDLE)) {
        Ok(tls) => {
            https::https_get(
                &tls,
                url,
                &["User-Agent: pulsar-update-probe"],
                64 << 20,
                None,
                None,
                5,
            )
            .await
        }
        Err(error) => HttpsResponse {
            error,
            ..Default::default()
        },
    };
    let error = if r.error.is_empty() { "" } else { ", error: " };
    out(format!(
        "status {}, {} bytes, sha256 {}{error}{}\n",
        r.status,
        r.body.len(),
        https::sha256_hex(&r.body),
        r.error
    )
    .as_bytes());
    if r.status != 200 {
        // printf("%.400s"): up to 400 bytes, cut at a NUL.
        let body = &r.body[..r.body.len().min(400)];
        let text = body.split(|&b| b == 0).next().unwrap_or_default();
        out(text);
        out(b"\n");
    }
    if r.status == 200 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn check(repo: &str) -> ExitCode {
    let (up, mut events) = pulsar_update::spawn(repo.into(), "probe".into());
    up.check();
    while let Some(ev) = events.recv().await {
        if let UpdateEvent::Checked { result, .. } = ev {
            return match result {
                Ok(r) => {
                    out(format!(
                        "version {}\nurl {}\nbin {}\nsums {}\n",
                        r.version, r.url, r.bin_url, r.sums_url
                    )
                    .as_bytes());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    out(format!("error: {e}\n").as_bytes());
                    ExitCode::FAILURE
                }
            };
        }
    }
    ExitCode::FAILURE
}

async fn install(args: &[String], binary: PathBuf) -> ExitCode {
    let [version, bin_url, sums_url] = args else {
        return ExitCode::from(2);
    };
    let (up, mut events) = pulsar_update::spawn(REPO.into(), "probe".into());
    let r = Release {
        version: version.clone(),
        url: String::new(),
        bin_url: bin_url.clone(),
        sums_url: sums_url.clone(),
    };
    up.install(r, binary);
    while let Some(ev) = events.recv().await {
        match ev {
            UpdateEvent::Progress { value, .. } => out(format!("progress {value:.0}\n").as_bytes()),
            UpdateEvent::Installed { result, .. } => {
                return match result {
                    Ok(()) => {
                        out(b"installed\n");
                        ExitCode::SUCCESS
                    }
                    Err((e, fatal)) => {
                        out(format!("error (fatal={}): {e}\n", u8::from(fatal)).as_bytes());
                        ExitCode::FAILURE
                    }
                };
            }
            UpdateEvent::Checked { .. } => {}
        }
    }
    ExitCode::FAILURE
}
