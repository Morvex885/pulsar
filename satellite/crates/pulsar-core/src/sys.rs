//! Every libc call of the crate.

use std::ffi::{CStr, CString};
use std::net::Ipv4Addr;
use std::os::fd::RawFd;
use std::sync::LazyLock;

/// Sets a process-wide environment variable. Call only before threads or libraries that read the
/// environment are started (Rust 2024 marks setenv unsafe for that reason).
pub fn set_var(key: &str, value: &str) {
    // SAFETY: the daemon calls this before spawning any thread or loading a library.
    unsafe { std::env::set_var(key, value) };
}

#[allow(
    clippy::useless_conversion,
    reason = "time_t and long are 32-bit on armhf, 64-bit on aarch64"
)]
pub fn monotonic_ms() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    i64::from(ts.tv_sec) * 1000 + i64::from(ts.tv_nsec) / 1_000_000
}

/// Local wall-clock hour, minute and second of a Unix time, in the process's TZ.
pub fn local_hms(unix_secs: i64) -> (i32, i32, i32) {
    let t = unix_secs as libc::time_t;
    // SAFETY: an all-zero `tm` is a valid value (integers and a nullable `tm_zone` pointer).
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers refer to live locals; localtime_r is the thread-safe variant.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return (0, 0, 0);
    }
    (tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// The "C" locale, created once; null if newlocale failed. Kept as an address: `locale_t` is a
/// raw pointer, and the object is immutable and never freed.
fn c_locale() -> libc::locale_t {
    static LOC: LazyLock<usize> = LazyLock::new(|| {
        // SAFETY: valid mask and NUL-terminated name; a null base asks for a fresh object.
        let loc =
            unsafe { libc::newlocale(libc::LC_NUMERIC_MASK, c"C".as_ptr(), std::ptr::null_mut()) };
        loc as usize
    });
    *LOC as libc::locale_t
}

/// Runs `f` with this thread on the "C" locale. GStreamer and the vendor libraries may call
/// setlocale; state.conf must keep '.' as the decimal point for the C++ binary (`.prev`).
fn in_c_locale<T>(f: impl FnOnce() -> T) -> T {
    let loc = c_locale();
    if loc.is_null() {
        return f();
    }
    // SAFETY: `loc` is a live locale object from newlocale; uselocale only affects this thread.
    let old = unsafe { libc::uselocale(loc) };
    let r = f();
    if !old.is_null() {
        // SAFETY: `old` is the thread's previous locale (or LC_GLOBAL_LOCALE) returned above.
        unsafe { libc::uselocale(old) };
    }
    r
}

/// printf("%g", v) in the "C" locale.
pub fn format_g(v: f64) -> String {
    // "%g" is at most 6 significant digits plus sign, point and a 4-character exponent;
    // "-nan" / "-inf" are shorter.
    let mut buf = [0u8; 32];
    let n = in_c_locale(|| {
        // SAFETY: the buffer is writable for its full length, which is passed as the size
        // limit; the format consumes exactly one double, which is passed.
        unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"%g".as_ptr(), v) }
    });
    let n = usize::try_from(n).unwrap_or(0).min(buf.len() - 1);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// strtof(s, nullptr) in the "C" locale: the longest numeric prefix (after leading white
/// space), else 0.
pub fn strtof(s: &str) -> f32 {
    // strtof stops at a NUL anyway; cut there so the CString cannot fail.
    let s = s.split('\0').next().unwrap_or("");
    let Ok(c) = CString::new(s) else { return 0.0 };
    // SAFETY: `c` is a NUL-terminated string that outlives the call; a null endptr is allowed.
    in_c_locale(|| unsafe { libc::strtof(c.as_ptr(), std::ptr::null_mut()) })
}

/// One line to fd 2 with write(2), bypassing std's stderr lock. A log line is short enough
/// for one write; short writes and EINTR are retried anyway.
pub fn write_stderr(mut buf: &[u8]) {
    while !buf.is_empty() {
        // SAFETY: `buf` is valid for reads of `buf.len()` bytes.
        let n = unsafe { libc::write(2, buf.as_ptr().cast(), buf.len()) };
        match usize::try_from(n) {
            Ok(0) => return,
            Ok(n) => buf = &buf[n..],
            Err(_) if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
            }
            Err(_) => return,
        }
    }
}

// The libc crate does not bind tzset; glibc exports it.
unsafe extern "C" {
    fn tzset();
}

/// Sets TZ and re-reads it for localtime_r.
pub fn set_tz(posix_tz: &str) {
    // SAFETY: the environment is not synchronized with other threads (GLib reads it). The
    // caller only sets TZ when the value changes, which is the same exposure the C++ daemon had.
    unsafe { std::env::set_var("TZ", posix_tz) };
    // SAFETY: tzset has no preconditions.
    unsafe { tzset() };
}

/// pwrite(2); returns the byte count or -1.
pub fn pwrite(fd: RawFd, buf: &[u8], offset: i64) -> isize {
    // SAFETY: `buf` is valid for reads of `buf.len()` bytes; an invalid fd only yields EBADF.
    unsafe { libc::pwrite(fd, buf.as_ptr().cast(), buf.len(), offset as libc::off_t) }
}

/// One IPv4 entry of getifaddrs.
pub struct IfAddr {
    pub name: String,
    pub flags: u32,
    pub addr: Ipv4Addr,
}

/// IPv4 addresses of all interfaces; None when getifaddrs fails.
pub fn ipv4_ifaddrs() -> Option<Vec<IfAddr>> {
    let mut ifs: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `ifs` is a valid out-pointer; the list is freed below.
    if unsafe { libc::getifaddrs(&mut ifs) } != 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut a = ifs;
    while !a.is_null() {
        // SAFETY: `a` is a non-null node of the list getifaddrs returned, not yet freed.
        let ifa = unsafe { &*a };
        a = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: `ifa_addr` is non-null and points to at least a sockaddr header.
        if i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: the family is AF_INET, so the address is a sockaddr_in.
        let sin = unsafe { &*ifa.ifa_addr.cast::<libc::sockaddr_in>() };
        // SAFETY: `ifa_name` is a NUL-terminated string owned by the list.
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) };
        out.push(IfAddr {
            name: name.to_string_lossy().into_owned(),
            flags: ifa.ifa_flags,
            addr: Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
        });
    }
    // SAFETY: `ifs` came from a successful getifaddrs and no reference into it survives.
    unsafe { libc::freeifaddrs(ifs) };
    Some(out)
}

/// if_nametoindex(3); 0 when the interface does not exist.
pub fn if_nametoindex(name: &str) -> u32 {
    let Ok(c) = CString::new(name) else { return 0 };
    // SAFETY: `c` is a NUL-terminated string that outlives the call.
    unsafe { libc::if_nametoindex(c.as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decimal-comma locale on this test thread only (uselocale), as GStreamer's setlocale
    /// would give the whole process. Skipped where ru_RU.UTF-8 is not installed.
    #[test]
    fn numbers_ignore_a_comma_locale() {
        // SAFETY: valid mask and NUL-terminated name; a null base asks for a fresh object.
        let ru = unsafe {
            libc::newlocale(
                libc::LC_ALL_MASK,
                c"ru_RU.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if ru.is_null() {
            eprintln!("ru_RU.UTF-8 not installed: skipped");
            return;
        }
        // SAFETY: `ru` is a live locale object; uselocale only affects this thread.
        let old = unsafe { libc::uselocale(ru) };
        let mut buf = [0u8; 32];
        // SAFETY: as in format_g.
        let n =
            unsafe { libc::snprintf(buf.as_mut_ptr().cast(), buf.len(), c"%g".as_ptr(), 0.5f64) };
        let raw = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
        let formatted = format_g(0.5);
        let parsed = strtof("0.5");
        // SAFETY: restores this thread's previous locale, then frees `ru`, no longer in use.
        unsafe {
            libc::uselocale(old);
            libc::freelocale(ru);
        }
        assert_eq!(raw, "0,5", "the locale really uses a decimal comma");
        assert_eq!(formatted, "0.5");
        assert_eq!(parsed, 0.5);
    }
}
