//! `update_probe` output against the C++ probe. tests/golden/versions.txt is the stdout of the
//! C++ `update_probe versions` (tag rust-port-base, built for the host with zig c++).

use std::process::Command;

fn probe(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_update_probe"))
        .args(args)
        .output()
        .expect("run update_probe")
}

#[test]
fn versions_table_matches_the_cxx_probe() {
    let out = probe(&["versions"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        include_str!("golden/versions.txt")
    );
}

#[test]
fn usage() {
    for args in [
        &[][..],
        &["get"],
        &["check", "a", "b"],
        &["install", "v1"],
        &["bogus"],
    ] {
        let out = probe(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.ends_with(
                " versions | get <url> | check [owner/repo] | install <version> <bin_url> <sums_url> <binary>\n"
            ),
            "{err}"
        );
    }
}
