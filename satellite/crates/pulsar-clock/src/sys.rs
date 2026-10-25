//! Every libc call of the crate.

/// Local wall-clock time for the display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wall {
    pub hour: i32,
    pub min: i32,
    /// Within the first half of the current second.
    pub first_half: bool,
}

/// CLOCK_REALTIME broken down by localtime_r, i.e. in the process TZ (the stations have no
/// /etc/localtime or zoneinfo: without TZ from Home Assistant this is UTC).
pub fn wall() -> Wall {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec; CLOCK_REALTIME always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
    let mut tm = zeroed_tm();
    // SAFETY: both pointers refer to live locals; localtime_r is the thread-safe variant.
    if unsafe { libc::localtime_r(&ts.tv_sec, &mut tm) }.is_null() {
        // Out of range for the broken-down time: shown as 00:00.
        tm = zeroed_tm();
    }
    Wall {
        hour: tm.tm_hour,
        min: tm.tm_min,
        first_half: ts.tv_nsec < 500_000_000,
    }
}

fn zeroed_tm() -> libc::tm {
    // SAFETY: an all-zero `tm` is a valid value (integers and a nullable `tm_zone` pointer).
    unsafe { std::mem::zeroed() }
}
