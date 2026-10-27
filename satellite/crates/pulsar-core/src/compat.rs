//! The Mini's glibc 2.23 has no `getrandom` (glibc 2.25). Rust's standard library references it
//! for `/dev/urandom` replacement; provide the syscall wrapper on 32-bit arm, whose build targets
//! that older ABI. The aarch64 build (glibc 2.36) uses the libc symbol. Every armhf binary and
//! test harness links pulsar-core, so the shim is reachable from one place.

#[cfg(all(target_arch = "arm", target_os = "linux"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrandom(buf: *mut std::ffi::c_void, buflen: usize, flags: u32) -> isize {
    // SAFETY: the caller (libc / Rust std) owns a writable buffer of `buflen` bytes; the syscall
    // has the same contract as glibc's getrandom.
    unsafe { libc::syscall(libc::SYS_getrandom, buf, buflen, flags) as isize }
}
