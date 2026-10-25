//! Small reads and writes of sysfs / procfs attributes.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::fd::RawFd;

use crate::sys;

/// Whole contents of a small file; None on any error.
pub fn read_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// One write(2) of `val` to an existing file (no create, no truncate): a sysfs attribute takes
/// a value per write call, so a short write is a failure rather than something to resume.
pub fn write_file(path: &str, val: &str) -> bool {
    let Ok(mut f) = OpenOptions::new().write(true).open(path) else {
        return false;
    };
    matches!(f.write(val.as_bytes()), Ok(n) if n == val.len())
}

/// Writes `v` in decimal to an attribute kept open (LED brightness, GPIO value) at offset 0,
/// which sysfs needs for every store on a reused fd.
pub fn write_int_fd(fd: RawFd, v: i32) -> bool {
    let mut buf = [0u8; 12]; // "-2147483648"
    let mut rest = &mut buf[..];
    if write!(rest, "{v}").is_err() {
        return false;
    }
    let len = 12 - rest.len();
    usize::try_from(sys::pwrite(fd, &buf[..len], 0)).is_ok_and(|n| n == len)
}

/// Drops the trailing newline (and spaces / CR) sysfs values end with.
pub fn trim(s: &str) -> &str {
    s.trim_end_matches(['\n', ' ', '\r'])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_only_trailing_newline_space_cr() {
        assert_eq!(trim("255\n"), "255");
        assert_eq!(trim(" 12 \r\n"), " 12");
        assert_eq!(trim("a\tb\t"), "a\tb\t");
    }
}
