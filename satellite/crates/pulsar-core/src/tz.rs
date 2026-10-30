//! System wall clock and time zone of the clock display and log time stamps.

/// Sets the system UTC epoch received from HA, independently of the selected timezone.
/// Fails if the epoch does not fit the platform's time_t or clock_settime is denied.
pub fn set_epoch(epoch: u32) -> std::io::Result<()> {
    crate::sys::set_realtime(epoch)
}

/// Sets TZ (POSIX form, e.g. "MSK-3") for localtime. Call only when the value changes: the
/// environment write races with GLib threads reading it, as it did in the C++ daemon.
pub fn set_tz(posix_tz: &str) {
    crate::sys::set_tz(posix_tz);
}
