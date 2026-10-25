//! Every libc call of the crate.

use std::io;

/// Fills `buf` from the kernel's CSPRNG (blocks only until the pool is initialised at boot);
/// false if the kernel refuses.
pub fn getrandom(buf: &mut [u8]) -> bool {
    let mut done = 0;
    while done < buf.len() {
        let rest = &mut buf[done..];
        // SAFETY: `rest` is a live, writable buffer of `rest.len()` bytes; flags 0 = urandom pool.
        let n = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        match usize::try_from(n) {
            Ok(n) if n > 0 => done += n,
            Ok(_) => return false,
            // A signal before any byte was copied.
            Err(_) if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}
