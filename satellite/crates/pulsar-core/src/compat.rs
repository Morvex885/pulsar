//! The armhf build targets glibc 2.23 (the Mini's userland), which has no `getrandom` (glibc
//! 2.25), yet Rust's standard library references it. This is the syscall wrapper for every 32-bit
//! ARM binary and test harness (all of them link pulsar-core). On the 2.30 userlands (Lite 2,
//! Mini 2, armhf Midi images) the binary still resolves to this definition, the same syscall as
//! libc's. The aarch64 build (glibc 2.36) uses libc's symbol.

#[cfg(all(target_arch = "arm", target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrandom(buf: *mut std::ffi::c_void, buflen: usize, flags: u32) -> isize {
    // SAFETY: the caller (libc / Rust std) owns a writable buffer of `buflen` bytes; the syscall
    // has the same contract as glibc's getrandom.
    unsafe { libc::syscall(libc::SYS_getrandom, buf, buflen, flags) as isize }
}
