//! libc calls of the updater.

/// sync(2): the swapped binary and its `.prev` reach the flash before the daemon exits (a power
/// cut right after an update must not leave a truncated binary).
pub fn sync() {
    // SAFETY: sync() takes no arguments, touches no memory of this process and cannot fail.
    unsafe { libc::sync() }
}
