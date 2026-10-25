//! Crash report straight to stderr (the log file), same format as the C++ daemon: everything in
//! the handler is async-signal-safe (no allocation, no std::io). The deployed binary is stripped;
//! resolve the addresses with `addr2line -f -e build/<arch>/pulsar.debug` (non-PIE: addresses are
//! absolute). glibc's backtrace() is not used: it loads libgcc_s lazily on its first call, and on
//! armhf libgcc_s's unwinder ran into the C++ daemon's static personality routine and
//! segfaulted; aarch64 walks the frame-pointer chain instead (-C force-frame-pointers=yes),
//! armhf reports pc/lr only.

use std::cell::UnsafeCell;
use std::ffi::{c_int, c_void};
use std::ptr;

const ALTSTACK_SIZE: usize = 64 * 1024;

/// Signal stack of the main thread; only the kernel writes to it, while a handler runs on it.
struct AltStack(UnsafeCell<[u8; ALTSTACK_SIZE]>);

// SAFETY: Rust code never reads or writes the buffer; it is only handed to sigaltstack once.
unsafe impl Sync for AltStack {}

static ALTSTACK: AltStack = AltStack(UnsafeCell::new([0; ALTSTACK_SIZE]));

// The register fields read below, checked against the kernel's sigcontext / glibc's ucontext_t.
#[cfg(target_arch = "aarch64")]
const _: () = {
    assert!(core::mem::offset_of!(libc::ucontext_t, uc_mcontext) == 176);
    assert!(core::mem::offset_of!(libc::mcontext_t, regs) == 8);
    assert!(core::mem::offset_of!(libc::mcontext_t, pc) == 264);
};
#[cfg(target_arch = "arm")]
const _: () = {
    assert!(core::mem::offset_of!(libc::ucontext_t, uc_mcontext) == 20);
    assert!(core::mem::offset_of!(libc::mcontext_t, arm_lr) == 68);
    assert!(core::mem::offset_of!(libc::mcontext_t, arm_pc) == 72);
};

fn put(s: &[u8]) {
    // SAFETY: write(2) is async-signal-safe; the slice is live.
    let _ = unsafe { libc::write(2, s.as_ptr().cast(), s.len()) };
}

/// `<label>0x<hex> `: no leading zeros, a trailing space.
fn put_hex(label: &[u8], v: usize) {
    let mut b = [0u8; 40];
    let mut n = 0;
    for &c in label {
        b[n] = c;
        n += 1;
    }
    b[n] = b'0';
    b[n + 1] = b'x';
    n += 2;
    let mut started = false;
    let mut shift = usize::BITS as i32 - 4;
    while shift >= 0 {
        let d = (v >> shift) & 0xf;
        if d != 0 || started || shift == 0 {
            started = true;
            b[n] = b"0123456789abcdef"[d];
            n += 1;
        }
        shift -= 4;
    }
    b[n] = b' ';
    put(&b[..=n]);
}

/// SIGSEGV, SIGBUS, SIGABRT (also `panic = "abort"`), SIGFPE, SIGILL.
#[unsafe(no_mangle)]
pub extern "C" fn pulsar_crash_handler(sig: c_int, info: *mut libc::siginfo_t, ctx: *mut c_void) {
    // Whatever below hangs, SIGALRM (default action) ends the process and the supervisor
    // restarts it.
    // SAFETY: alarm is async-signal-safe.
    unsafe { libc::alarm(3) };
    put(b"\n=== CRASH ");
    put(match sig {
        libc::SIGSEGV => b"SIGSEGV ".as_slice(),
        libc::SIGBUS => b"SIGBUS ",
        libc::SIGABRT => b"SIGABRT ",
        _ => b"signal ",
    });
    let addr = if info.is_null() {
        0
    } else {
        // SAFETY: the kernel passes a valid siginfo_t to an SA_SIGINFO handler.
        unsafe { (*info).si_addr() as usize }
    };
    put_hex(b"addr=", addr);
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    registers(ctx.cast());
    put_hex(b"handler=", pulsar_crash_handler as *const () as usize);
    put(b"\n");
    #[cfg(target_arch = "aarch64")]
    frame_pointers(ctx.cast());
    // Other architectures (the host build for tests) report no registers.
    #[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
    let _ = ctx;
    // SAFETY: signal and raise are async-signal-safe; the default action ends the process.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

#[cfg(target_arch = "aarch64")]
fn registers(uc: *const libc::ucontext_t) {
    if uc.is_null() {
        return;
    }
    // SAFETY: the kernel passes the interrupted context to an SA_SIGINFO handler.
    let mc = unsafe { &(*uc).uc_mcontext };
    put_hex(b"pc=", mc.pc as usize);
    put_hex(b"lr=", mc.regs[30] as usize);
}

#[cfg(target_arch = "arm")]
fn registers(uc: *const libc::ucontext_t) {
    if uc.is_null() {
        return;
    }
    // SAFETY: the kernel passes the interrupted context to an SA_SIGINFO handler.
    let mc = unsafe { &(*uc).uc_mcontext };
    put_hex(b"pc=", mc.arm_pc as usize);
    put_hex(b"lr=", mc.arm_lr as usize);
}

/// `fp: <ret> <ret> ...`: x29 points at {previous x29, return address}; up to 40 frames, stops at
/// a null, misaligned or non-increasing frame pointer or a null return address.
#[cfg(target_arch = "aarch64")]
fn frame_pointers(uc: *const libc::ucontext_t) {
    if uc.is_null() {
        return;
    }
    // SAFETY: the kernel passes the interrupted context to an SA_SIGINFO handler.
    let mut fp = unsafe { (*uc).uc_mcontext.regs[29] } as usize;
    put(b"fp: ");
    let mut i = 0;
    while i < 40 && fp != 0 && fp & 15 == 0 {
        let frame = fp as *const usize;
        // SAFETY: a 16-byte aligned frame record of the crashed thread's stack; a chain corrupted
        // into unmapped memory faults again, and SA_RESETHAND makes that fault end the process.
        let (next, ret) = unsafe { (ptr::read_volatile(frame), ptr::read_volatile(frame.add(1))) };
        if ret == 0 {
            break;
        }
        put_hex(b"", ret);
        if next <= fp {
            break;
        }
        fp = next;
        i += 1;
    }
    put(b"\n");
}

/// Handlers for the crash signals, on their own stack so that a stack overflow still produces a
/// report.
pub fn install() {
    let ss = libc::stack_t {
        ss_sp: ALTSTACK.0.get().cast(),
        ss_flags: 0,
        ss_size: ALTSTACK_SIZE,
    };
    // SAFETY: the stack is a static, so it outlives every signal delivered on this thread.
    unsafe { libc::sigaltstack(&ss, ptr::null_mut()) };
    // SAFETY: an all-zero sigaction is a valid value (empty mask, no flags, no handler).
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = pulsar_crash_handler as *const () as libc::sighandler_t;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND | libc::SA_NODEFER;
    // SAFETY: sa_mask is a live sigset_t.
    unsafe { libc::sigemptyset(&mut sa.sa_mask) };
    for sig in [
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGABRT,
        libc::SIGFPE,
        libc::SIGILL,
    ] {
        // SAFETY: valid signal number and action; the handler has the SA_SIGINFO signature.
        unsafe { libc::sigaction(sig, &sa, ptr::null_mut()) };
    }
}
