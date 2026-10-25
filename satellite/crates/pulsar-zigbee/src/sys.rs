//! Every libc call of the crate: the radio's serial port (termios, poll) and, for tests, a
//! pseudo-terminal standing in for it.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Opens a serial port read-write, never as the controlling terminal (std adds O_CLOEXEC):
/// non-blocking for the link, which reads through the runtime; blocking for the flasher, which
/// polls before every read.
pub fn open_tty(path: &str, nonblocking: bool) -> io::Result<File> {
    let flags = libc::O_NOCTTY | if nonblocking { libc::O_NONBLOCK } else { 0 };
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(flags)
        .open(path)
}

/// ENOTTY unless `f` is a terminal.
pub fn ensure_tty(f: &File) -> io::Result<()> {
    // SAFETY: isatty only inspects the descriptor, which `f` keeps open.
    if unsafe { libc::isatty(f.as_raw_fd()) } == 1 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::ENOTTY))
    }
}

/// Raw 8N1 at 115200 without flow control. `vmin_zero`: VMIN = VTIME = 0, so a read returns
/// whatever is there (the link); otherwise cfmakeraw's VMIN = 1 (the flasher polls first).
pub fn set_raw_115200(f: &File, vmin_zero: bool) -> io::Result<()> {
    let fd = f.as_raw_fd();
    // SAFETY: termios holds only integers; all zero is a valid value. Value-initialised as in
    // C++, so a failing tcgetattr (not a terminal) leaves zeros and tcsetattr reports the error.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open for the duration of the call and `t` is a writable termios.
    unsafe { libc::tcgetattr(fd, &mut t) };
    // SAFETY: `t` is a valid termios; these only rewrite its fields.
    unsafe {
        libc::cfmakeraw(&mut t);
        libc::cfsetispeed(&mut t, libc::B115200);
        libc::cfsetospeed(&mut t, libc::B115200);
    }
    t.c_cflag |= libc::CLOCAL | libc::CREAD;
    t.c_cflag &= !(libc::CSTOPB | libc::CRTSCTS);
    if vmin_zero {
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 0;
    }
    // SAFETY: `fd` is open and `t` is a valid termios.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Hands the link's tty to the runtime for read readiness (writes never wait for it).
pub fn register_readable(f: File) -> io::Result<AsyncFd<File>> {
    // SAFETY: `File` owns its descriptor, so it stays open and the same for the AsyncFd's whole
    // lifetime, and as_raw_fd always returns it.
    Ok(unsafe { AsyncFd::register_with_interest(f, Interest::READABLE) }?)
}

/// Drops whatever the port has buffered in both directions.
pub fn flush_io(f: &File) {
    // SAFETY: tcflush only acts on the descriptor, which `f` keeps open.
    unsafe { libc::tcflush(f.as_raw_fd(), libc::TCIOFLUSH) };
}

/// Waits up to `ms` for input. false on timeout and on any poll error, EINTR included.
pub fn wait_readable(f: &File, ms: i32) -> bool {
    let mut p = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd, and nfds says one.
    unsafe { libc::poll(&mut p, 1, ms) > 0 }
}

/// A pseudo-terminal pair for tests: the master end and the path of the slave (a real tty for
/// the flasher's checks).
#[cfg(test)]
pub fn open_pty() -> io::Result<(File, String)> {
    use std::ffi::CStr;
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: plain call with valid flags; the result is checked.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    let master = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    // SAFETY: `fd` is an open pty master (kept open by `master`).
    if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: the buffer is writable for the length passed.
    if unsafe { libc::ptsname_r(fd, name.as_mut_ptr(), name.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ptsname_r succeeded, so the buffer holds a NUL-terminated path.
    let name = unsafe { CStr::from_ptr(name.as_ptr()) };
    Ok((master, name.to_string_lossy().into_owned()))
}
