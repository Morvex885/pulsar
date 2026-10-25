//! Every libc call of the crate: framebuffer and evdev ioctls, non-blocking evdev reads. The
//! ioctl numbers mean these requests only on their own device types, so they are methods of
//! `Framebuffer` and `Evdev`, which can only be created by opening such a node.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileExt as _, OpenOptionsExt as _};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// `struct fb_var_screeninfo` (linux/fb.h): 40 32-bit fields. Only handed back to the driver
/// (FBIOPAN_DISPLAY takes the offsets read by FBIOGET_VSCREENINFO), never interpreted.
#[repr(C)]
#[derive(Clone, Copy)]
struct FbVarScreeninfo([u32; 40]);

const _: () = assert!(size_of::<FbVarScreeninfo>() == 160);

const FBIOGET_VSCREENINFO: libc::Ioctl = 0x4600;
const FBIOPAN_DISPLAY: libc::Ioctl = 0x4606;

/// An open framebuffer device (/dev/fbN) with its variable screen info.
pub struct Framebuffer {
    file: File,
    var: FbVarScreeninfo,
}

impl Framebuffer {
    /// Opens `path` read-write; the caller has checked by its sysfs name that it is the
    /// expected framebuffer. FBIOGET_VSCREENINFO failing keeps a zeroed screen info, as in the
    /// C++ daemon.
    pub fn open(path: &str) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        let mut var = FbVarScreeninfo([0; 40]);
        // SAFETY: `file` is a framebuffer device (Framebuffer is only created here, for an fb
        // node), for which this request writes one fb_var_screeninfo; `var` is a writable value
        // of the kernel's size.
        unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                FBIOGET_VSCREENINFO,
                std::ptr::from_mut(&mut var),
            )
        };
        Ok(Self { file, var })
    }

    /// pwrite(2) at offset 0.
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        self.file.write_at(buf, 0)
    }

    /// FBIOPAN_DISPLAY: makes the ledfb driver push the framebuffer to the LEDs.
    pub fn pan(&mut self) {
        // SAFETY: as in open(): a framebuffer device, which reads (and may update) the
        // fb_var_screeninfo `var` points to.
        unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                FBIOPAN_DISPLAY,
                std::ptr::from_mut(&mut self.var),
            )
        };
    }
}

/// The kernel's `struct input_event` for this userland: `__kernel_ulong_t` seconds and
/// microseconds, then type, code, value.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawInputEvent {
    pub sec: libc::c_ulong,
    pub usec: libc::c_ulong,
    pub type_: u16,
    pub code: u16,
    pub value: i32,
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
const _: () = {
    assert!(size_of::<RawInputEvent>() == 24);
    assert!(core::mem::offset_of!(RawInputEvent, type_) == 16);
    assert!(core::mem::offset_of!(RawInputEvent, value) == 20);
};
#[cfg(target_arch = "arm")]
const _: () = {
    assert!(size_of::<RawInputEvent>() == 16);
    assert!(core::mem::offset_of!(RawInputEvent, type_) == 8);
    assert!(core::mem::offset_of!(RawInputEvent, value) == 12);
};

pub const EMPTY_EVENT: RawInputEvent = RawInputEvent {
    sec: 0,
    usec: 0,
    type_: 0,
    code: 0,
    value: 0,
};

/// `_IOC(_IOC_READ, 'E', nr, len)` of linux/input.h.
const fn evioc_read(nr: u32, len: usize) -> libc::Ioctl {
    ((2u32 << 30) | ((len as u32) << 16) | ((b'E' as u32) << 8) | nr) as libc::Ioctl
}

/// Key state bitmap (KEY_MAX = 0x2ff), little-endian bit order.
pub type KeyBits = [u8; 0x300 / 8];
/// Switch state bitmap (SW_MAX = 0x10), padded to one long like the kernel copies it.
pub type SwBits = [u8; 8];

/// An open evdev node (/dev/input/eventN), read-only and non-blocking.
pub struct Evdev(File);

impl AsRawFd for Evdev {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl Evdev {
    /// Opens `path`, an event node the caller found in /proc/bus/input/devices.
    pub fn open(path: &str) -> io::Result<Self> {
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map(Self)
    }

    /// Registers the device with the runtime's reactor for reading.
    pub fn into_async(self) -> io::Result<AsyncFd<Evdev>> {
        // SAFETY: the AsyncFd owns the file and never replaces it, so its descriptor stays open
        // and refers to the same file description until the AsyncFd is dropped.
        Ok(unsafe { AsyncFd::register_with_interest(self, Interest::READABLE) }?)
    }

    /// One read(2) of whole events; Ok(0) at end of file.
    pub fn read_events(&self, buf: &mut [RawInputEvent]) -> io::Result<usize> {
        // SAFETY: `buf` is writable for its full byte length; RawInputEvent is plain data, so
        // any bytes the kernel writes form valid values.
        let n = unsafe { libc::read(self.as_raw_fd(), buf.as_mut_ptr().cast(), size_of_val(buf)) };
        match usize::try_from(n) {
            Ok(n) => Ok(n / size_of::<RawInputEvent>()),
            Err(_) => Err(io::Error::last_os_error()),
        }
    }

    fn eviocg<const N: usize>(&self, nr: u32) -> io::Result<[u8; N]> {
        let mut bits = [0u8; N];
        // SAFETY: `self` is an evdev node (Evdev is only created by open()), for which this
        // request copies at most `N` bytes, the length encoded in it, into `bits`, which is
        // writable for `N` bytes.
        let r = unsafe { libc::ioctl(self.as_raw_fd(), evioc_read(nr, N), bits.as_mut_ptr()) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(bits)
        }
    }

    /// EVIOCGKEY: keys currently held.
    pub fn key_state(&self) -> io::Result<KeyBits> {
        self.eviocg(0x18)
    }

    /// EVIOCGSW: switch states.
    pub fn switch_state(&self) -> io::Result<SwBits> {
        self.eviocg(0x1b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_ioctl_numbers_match_the_kernel_macros() {
        // EVIOCGKEY(96) and EVIOCGSW(8) as computed by the C preprocessor.
        assert_eq!(evioc_read(0x18, 96) as u64, 0x8060_4518);
        assert_eq!(evioc_read(0x1b, 8) as u64, 0x8008_451b);
    }
}
