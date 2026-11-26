//! Every libc call of the binary outside the crash handler.

pub fn init_media_env() {
    // SAFETY: main вызывает это до загрузки библиотек, запуска рантайма и потоков.
    unsafe { pulsar_media::set_ssl_cert_file() };
}

/// Straight to fd 2, bypassing std's stderr lock (a hung teardown may hold it).
pub fn write_stderr(msg: &[u8]) {
    // SAFETY: the pointer and length describe a live byte slice.
    let _ = unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
}

/// Leaves without running exit handlers or destructors.
pub fn exit_now(status: i32) -> ! {
    // SAFETY: _exit has no preconditions.
    unsafe { libc::_exit(status) }
}
