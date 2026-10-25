//! Release tags are vMAJOR.MINOR.PATCH. A build describes itself (`git describe`) as the tag, as
//! `<tag>-N-g<hash>[-dirty]` after it, or as a bare hash without tags.

/// MAJOR.MINOR.PATCH of `s`, which is the version itself or has a `-…` suffix. The numbers are
/// read like C `sscanf("%d.%d.%d")` after an optional leading `v` (C++ `parse_version`).
fn parse_version(s: &str) -> Option<[i32; 3]> {
    let s = s.as_bytes();
    let mut pos = usize::from(s.first() == Some(&b'v'));
    let mut v = [0; 3];
    for (i, n) in v.iter_mut().enumerate() {
        if i > 0 {
            if s.get(pos) != Some(&b'.') {
                return None;
            }
            pos += 1;
        }
        *n = scan_int(s, &mut pos)?;
    }
    match s.get(pos) {
        None | Some(b'-') => Some(v),
        Some(_) => None,
    }
}

/// `%d`: leading C whitespace, an optional sign, at least one digit; saturates at the `i32`
/// bounds (out of range is undefined for sscanf).
fn scan_int(s: &[u8], pos: &mut usize) -> Option<i32> {
    let mut p = *pos;
    while s
        .get(p)
        .is_some_and(|&c| c == b' ' || (b'\t'..=b'\r').contains(&c))
    {
        p += 1;
    }
    let neg = s.get(p) == Some(&b'-');
    if matches!(s.get(p), Some(b'-' | b'+')) {
        p += 1;
    }
    let start = p;
    let mut n: i64 = 0;
    while let Some(d) = s.get(p).filter(|c| c.is_ascii_digit()) {
        n = (n * 10 + i64::from(d - b'0')).min(i64::from(i32::MAX) + 1);
        p += 1;
    }
    if p == start {
        return None;
    }
    *pos = p;
    let n = if neg { -n } else { n };
    Some(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
}

/// `latest` is a newer release than `current`: `(newer, current_known)`. `current_known`:
/// `current` parsed; otherwise `latest != current` decides (and automatic installs are off).
pub fn release_newer(latest: &str, current: &str) -> (bool, bool) {
    let cur = parse_version(current);
    let known = cur.is_some();
    let Some(l) = parse_version(latest) else {
        return (false, known);
    };
    match cur {
        // The same release or a build after it is not older: only a greater triple is newer.
        Some(c) => (l > c, true),
        None => (latest != current, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_table_of_the_cxx_probe() {
        // The cases of C++ `update_probe versions` with its results (tests/golden/versions.txt).
        let cases = [
            ("v1.2.4", "v1.2.3", (true, true)),
            ("v1.2.3", "v1.2.3", (false, true)),
            ("v1.2.3", "v1.2.3-4-gabc1234", (false, true)),
            ("v1.3.0", "v1.2.3-4-gabc", (true, true)),
            ("v1.2.3", "v1.10.0", (false, true)),
            ("v1.2.3", "6ac30be", (true, false)),
            ("v2.0.0", "v1.9.9-dirty", (true, true)),
            ("junk", "v1.0.0", (false, true)),
        ];
        for (latest, current, want) in cases {
            assert_eq!(release_newer(latest, current), want, "{latest} {current}");
        }
    }

    #[test]
    fn parse_like_sscanf() {
        assert_eq!(parse_version("v1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_version("1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_version("v1.2.3-4-gabc"), Some([1, 2, 3]));
        assert_eq!(parse_version("v1.2.3-dirty"), Some([1, 2, 3]));
        // %d skips whitespace and takes a sign.
        assert_eq!(parse_version("v 1.+2.\t-3"), Some([1, 2, -3]));
        assert_eq!(parse_version("v1.2.3x"), None);
        assert_eq!(parse_version("v1.2.3.4"), None);
        assert_eq!(parse_version("v1.2"), None);
        assert_eq!(parse_version("vv1.2.3"), None);
        assert_eq!(parse_version("v1..3"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("v99999999999.0.0"), Some([i32::MAX, 0, 0]));
        // Only the triple counts: a build after the tag is not older than the tag.
        assert_eq!(release_newer("v1.2.3", "v1.2.2-7-gdeadbee"), (true, true));
        assert_eq!(release_newer("v1.2.3", "6ac30be-dirty"), (true, false));
        assert_eq!(release_newer("6ac30be", "6ac30be"), (false, false));
    }
}
