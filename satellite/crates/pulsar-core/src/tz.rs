//! Time zone of the local clock display and log time stamps.

/// Sets TZ (POSIX form, e.g. "MSK-3") for localtime. Call only when the value changes: the
/// environment write races with GLib threads reading it, as it did in the C++ daemon.
pub fn set_tz(posix_tz: &str) {
    crate::sys::set_tz(posix_tz);
}
