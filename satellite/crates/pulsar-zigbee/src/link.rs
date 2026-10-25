//! Serial link to the Station Midi's Zigbee co-processor: a Telink B91 on /dev/ttyS2 at 115200
//! 8N1, no flow control, running one of pulsar's firmwares (radio/) or Yandex's gateway.
//!
//! Frames are Telink zbhci: `0x55 | type BE16 | length BE16 | XOR of type, length and payload |
//! payload | 0xAA`. The firmware DMA-receives exactly one frame per buffer and answers every frame
//! with an ACK (type 0x8000, payload `[type BE16][status][0]`); a second frame sent before that
//! ACK is merged into the same buffer and rejected, so frames go out stop-and-wait.

use std::collections::VecDeque;
use std::fs::File;
use std::future::poll_fn;
use std::io::{self, Read as _, Write as _};
use std::task::{Context, Poll, Waker};

use log::{error, info, warn};
use pulsar_core::{Timers, sysfs};
use tokio::io::unix::AsyncFd;

use crate::{RadioLines, os_error_text, sys};

pub const ACK: u16 = 0x8000;
/// Sent by the application once it runs.
pub const BOOT_IND: u16 = 0x8907;
/// Crash report, then a restart.
pub const CRASH_IND: u16 = 0x8909;
/// The ACK normally follows within a few ms.
const ACK_TIMEOUT_MS: u64 = 300;
const TRIES: u32 = 3;
const FAILURES_BEFORE_RESET: u32 = 3;
const RESET_PULSE_MS: u64 = 200;
const BOOT_WAIT_MS: u64 = 3000;
/// Payload limit of the firmware's transmit path.
const MAX_FRAME: usize = 0xF9;
/// The crash report carries up to 4 KiB of RAM.
const MAX_PAYLOAD: usize = 0x1100;

/// What the ACK of a sent frame concerns: the sender's continuation (the C++ `AckCb` closures).
/// Frames sent without a tag produce no [`LinkEvent::Ack`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckTag {
    /// Coordinator 0x0A04 for this endpoint.
    Endpoint(u8),
    /// Coordinator 0x0A01 (form a network).
    Form,
    /// Coordinator 0x0A10 (APS data request) with this radio handle.
    Data(u8),
    /// Coordinator 0x0A06 (energy scan).
    EdScan,
    /// Router 0x0002 (network steering).
    Steer,
    /// Router 0x0005 (leave).
    Leave,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LinkEvent {
    /// Every frame from the chip except ACKs and boot indications.
    Frame { ty: u16, payload: Vec<u8> },
    /// ACK of a frame sent with a tag: 0 = accepted, > 0 firmware status (2 = unknown type),
    /// -1 = no ACK (retries exhausted, or dropped by a hardware reset).
    Ack { tag: AckTag, status: i32 },
    /// The chip (re)started its application: everything it held in RAM is gone.
    Boot,
}

/// Byte stream to the chip: the tty, or an in-memory pipe in tests.
trait Wire {
    /// Reads what is available, waiting until something is (Pending while there is nothing).
    /// Ok(0): the input is closed for good (hangup); Err: a read error. Either ends reading for
    /// the link's lifetime.
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>>;
    /// Writes without waiting; Ok(0) or WouldBlock when the kernel's buffer is full.
    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize>;
}

struct Tty(AsyncFd<File>);

impl Wire for Tty {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let r = guard.get_inner().read(buf);
            match r {
                Ok(n) if n > 0 => return Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // With VMIN = VTIME = 0 an empty tty reads as 0 bytes (n_tty), not EAGAIN: both
                // just mean "nothing now". Only the runtime's READ_CLOSED (final: a hangup
                // would stay "readable" for good) makes them the end of input.
                Ok(_) => {
                    if guard.ready().is_read_closed() {
                        return Poll::Ready(Ok(0));
                    }
                    guard.clear_ready();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if guard.ready().is_read_closed() {
                        return Poll::Ready(Ok(0));
                    }
                    guard.clear_ready();
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Not through the runtime's write readiness: the fd is non-blocking, and readiness is
        // only known after the driver's first turn.
        self.0.get_ref().write(buf)
    }
}

#[cfg(test)]
impl Wire for tokio::io::DuplexStream {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        use tokio::io::AsyncRead as _;
        let mut rb = tokio::io::ReadBuf::new(buf);
        match std::pin::Pin::new(self).poll_read(cx, &mut rb) {
            Poll::Ready(r) => Poll::Ready(r.map(|()| rb.filled().len())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn try_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use tokio::io::AsyncWrite as _;
        let mut cx = Context::from_waker(Waker::noop());
        match std::pin::Pin::new(self).poll_write(&mut cx, buf) {
            Poll::Ready(r) => r,
            Poll::Pending => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    Ack,
    Hold,
    /// The lines could not be driven: carry on as if the chip had booted.
    BootNow,
    ResetPulse,
    BootWait,
}

struct Out {
    ty: u16,
    frame: Vec<u8>,
    tag: Option<AckTag>,
    hold_for: u16,
    hold_ms: u64,
    tries: u32,
}

pub struct Link {
    wire: Box<dyn Wire>,
    tty: String,
    /// The wire hung up or failed: no more input, timers (ACKs, reset) keep running.
    read_dead: bool,
    reset_line: String,
    boot_line: String,
    timers: Timers<Timer>,
    rx: Vec<u8>,
    out: VecDeque<Out>,
    events: VecDeque<LinkEvent>,
    in_flight: bool,
    /// Reset pulse or boot wait in progress: nothing is sent.
    resetting: bool,
    /// Reset line asserted: a boot indication now predates the pulse.
    reset_held: bool,
    /// Frame type awaited before anything else is sent (0 = none).
    hold_for: u16,
    /// Consecutive frames without ACK.
    failures: u32,
}

impl Link {
    /// Opens the tty and hardware-resets the chip; [`LinkEvent::Boot`] follows once its
    /// application runs. Errors are logged. Must run inside the tokio runtime (the tty is
    /// registered with its reactor); from then on [`Link::next`] must be polled continuously,
    /// or the reset, ACK retries and holds do not advance.
    pub fn open(lines: &RadioLines) -> Option<Link> {
        let file = match sys::open_tty(&lines.tty, true) {
            Ok(f) => f,
            Err(e) => {
                error!("zigbee: open {}: {}", lines.tty, os_error_text(&e));
                return None;
            }
        };
        if let Err(e) = sys::set_raw_115200(&file, true) {
            error!("zigbee: configure {}: {}", lines.tty, os_error_text(&e));
            return None;
        }
        sys::flush_io(&file);
        let fd = match sys::register_readable(file) {
            Ok(fd) => fd,
            Err(e) => {
                error!("zigbee: open {}: {}", lines.tty, os_error_text(&e));
                return None;
            }
        };
        let mut link = Link::with_wire(Box::new(Tty(fd)), lines);
        link.reset();
        Some(link)
    }

    /// A link over the in-memory pipe `wire` (the test's radio holds the other end), reset as
    /// [`Link::open`] does.
    #[cfg(test)]
    pub(crate) fn over(wire: tokio::io::DuplexStream, lines: &RadioLines) -> Link {
        let mut link = Link::with_wire(Box::new(wire), lines);
        link.reset();
        link
    }

    fn with_wire(wire: Box<dyn Wire>, lines: &RadioLines) -> Link {
        Link {
            wire,
            tty: lines.tty.clone(),
            read_dead: false,
            reset_line: lines.reset.clone(),
            boot_line: lines.boot.clone(),
            timers: Timers::new(),
            rx: Vec::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
            in_flight: false,
            resetting: false,
            reset_held: false,
            hold_for: 0,
            failures: 0,
        }
    }

    /// Frames queued, the one in flight included.
    pub fn queued(&self) -> usize {
        self.out.len()
    }

    /// Queues a frame. With a tag, its ACK (or its absence) comes back as [`LinkEvent::Ack`].
    pub fn send(&mut self, ty: u16, payload: &[u8], tag: Option<AckTag>) {
        self.send_and_hold(ty, payload, tag, 0, 0);
    }

    /// As [`Link::send`]; after the ACK nothing else is sent until a frame of type `hold_for`
    /// arrives or `hold_ms` pass: for commands during which the firmware busy-waits and would
    /// miss further frames.
    pub fn send_and_hold(
        &mut self,
        ty: u16,
        payload: &[u8],
        tag: Option<AckTag>,
        hold_for: u16,
        hold_ms: u64,
    ) {
        self.out.push_back(Out {
            ty,
            frame: build_frame(ty, payload),
            tag,
            hold_for,
            hold_ms,
            tries: 0,
        });
        self.pump();
    }

    /// Hardware reset (reset line pulse with the boot line low). Queued frames fail with -1.
    pub fn reset(&mut self) {
        for t in [
            Timer::Ack,
            Timer::Hold,
            Timer::BootNow,
            Timer::ResetPulse,
            Timer::BootWait,
        ] {
            self.timers.cancel(t);
        }
        self.in_flight = false;
        self.hold_for = 0;
        self.resetting = true;
        for o in std::mem::take(&mut self.out) {
            if let Some(tag) = o.tag {
                self.events.push_back(LinkEvent::Ack { tag, status: -1 });
            }
        }
        // Boot line released: run the application, not the resident bootloader. Then a reset
        // pulse.
        if !sysfs::write_file(&self.boot_line, "0") || !sysfs::write_file(&self.reset_line, "1") {
            warn!(
                "zigbee: cannot drive {} / {}, radio not reset",
                self.reset_line, self.boot_line
            );
            self.timers.set(Timer::BootNow, 0);
            return;
        }
        self.reset_held = true;
        self.timers.set(Timer::ResetPulse, RESET_PULSE_MS);
    }

    /// Next event. Cancel-safe: input and timers are handled between awaits, and events wait in
    /// a queue until returned. The link's timers only run inside this call.
    pub async fn next(&mut self) -> LinkEvent {
        loop {
            if let Some(ev) = self.events.pop_front() {
                return ev;
            }
            let mut buf = [0u8; 512];
            // Input first, as the C++ loop ran fd callbacks before timers: an ACK that is
            // already there wins over its timeout.
            tokio::select! {
                biased;
                r = poll_fn(|cx| self.wire.poll_read(cx, &mut buf)), if !self.read_dead => {
                    match r {
                        Ok(n @ 1..) => {
                            self.rx.extend_from_slice(&buf[..n]);
                            self.read_available();
                            self.parse();
                        }
                        Ok(_) => self.stop_reading("hung up"),
                        Err(e) => self.stop_reading(&os_error_text(&e)),
                    }
                }
                t = self.timers.next() => self.on_timer(t),
            }
        }
    }

    /// Takes the rest of what the wire holds without waiting (the C++ read loop until EAGAIN).
    fn read_available(&mut self) {
        let mut buf = [0u8; 512];
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            match self.wire.poll_read(&mut cx, &mut buf) {
                Poll::Ready(Ok(n @ 1..)) => self.rx.extend_from_slice(&buf[..n]),
                Poll::Ready(Ok(_)) => return self.stop_reading("hung up"),
                Poll::Ready(Err(e)) => return self.stop_reading(&os_error_text(&e)),
                Poll::Pending => return,
            }
        }
    }

    /// A hung-up or failing tty would report input forever (the C++ loop spun on it): logged
    /// once, then only timers run, so ACKs time out and the reset logic carries on.
    fn stop_reading(&mut self, why: &str) {
        error!(
            "zigbee: read {}: {why}; no more input from the radio",
            self.tty
        );
        self.read_dead = true;
    }

    fn on_timer(&mut self, t: Timer) {
        match t {
            Timer::Ack => self.ack_timeout(),
            Timer::Hold => {
                warn!("zigbee: no 0x{:04x} from the radio", self.hold_for);
                self.release_hold();
            }
            Timer::BootNow => self.booted(),
            Timer::ResetPulse => {
                sysfs::write_file(&self.reset_line, "0");
                self.reset_held = false;
                self.rx.clear();
                self.timers.set(Timer::BootWait, BOOT_WAIT_MS);
            }
            Timer::BootWait => {
                warn!("zigbee: no boot indication from the radio, continuing");
                self.booted();
            }
        }
    }

    fn booted(&mut self) {
        for t in [Timer::BootNow, Timer::ResetPulse, Timer::BootWait] {
            self.timers.cancel(t);
        }
        self.resetting = false;
        self.failures = 0;
        self.events.push_back(LinkEvent::Boot);
        self.pump();
    }

    fn pump(&mut self) {
        if self.in_flight || self.resetting || self.hold_for != 0 {
            return;
        }
        let Some(o) = self.out.front_mut() else {
            return;
        };
        o.tries += 1;
        let mut left = &o.frame[..];
        while !left.is_empty() {
            match self.wire.try_write(left) {
                Ok(n @ 1..) => left = &left[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // Kernel tty buffer full: the ACK timeout retries the frame.
                _ => break,
            }
        }
        self.in_flight = true;
        self.timers.set(Timer::Ack, ACK_TIMEOUT_MS);
    }

    fn ack_timeout(&mut self) {
        self.in_flight = false;
        let Some(head) = self.out.front() else {
            return;
        };
        if head.tries < TRIES {
            return self.pump();
        }
        warn!("zigbee: no ACK for frame 0x{:04x}", head.ty);
        self.failures += 1;
        if self.failures >= FAILURES_BEFORE_RESET {
            error!("zigbee: radio not answering, resetting it");
            return self.reset();
        }
        self.finish_head(-1);
    }

    fn finish_head(&mut self, status: i32) {
        self.timers.cancel(Timer::Ack);
        self.in_flight = false;
        let Some(o) = self.out.pop_front() else {
            return;
        };
        if status >= 0 {
            self.failures = 0;
        }
        if status == 0 && o.hold_for != 0 {
            self.hold_for = o.hold_for;
            self.timers.set(Timer::Hold, o.hold_ms);
        }
        if let Some(tag) = o.tag {
            self.events.push_back(LinkEvent::Ack { tag, status });
        }
        self.pump();
    }

    fn release_hold(&mut self) {
        self.timers.cancel(Timer::Hold);
        self.hold_for = 0;
        self.pump();
    }

    fn parse(&mut self) {
        // Handlers never touch rx (only the reset pulse's timer clears it).
        let rx = std::mem::take(&mut self.rx);
        let used = scan_frames(&rx, |ty, p| self.handle(ty, p));
        self.rx = rx;
        self.rx.drain(..used);
    }

    fn handle(&mut self, ty: u16, p: &[u8]) {
        if ty == ACK {
            if p.len() < 3 || !self.in_flight {
                return;
            }
            let Some(head) = self.out.front() else {
                return;
            };
            let acked = u16::from_be_bytes([p[0], p[1]]);
            // 0xFFFF: the firmware could not even find the start byte; it concerns the frame in
            // flight.
            if acked != head.ty && acked != 0xFFFF {
                return;
            }
            if p[2] != 0 {
                warn!(
                    "zigbee: radio rejected frame 0x{:04x}: status 0x{:02x}",
                    head.ty, p[2]
                );
            }
            return self.finish_head(i32::from(p[2]));
        }
        if ty == BOOT_IND {
            if self.reset_held {
                return; // queued before the pulse; the real one follows its release
            }
            info!("zigbee: radio application started");
            if !self.resetting {
                // Spontaneous restart (watchdog or crash): what is in flight never gets its
                // ACK.
                self.timers.cancel(Timer::Ack);
                self.timers.cancel(Timer::Hold);
                self.in_flight = false;
                self.hold_for = 0;
                for o in &mut self.out {
                    o.tries = 0;
                }
            }
            return self.booted();
        }
        let releases = self.hold_for != 0 && ty == self.hold_for;
        self.events.push_back(LinkEvent::Frame {
            ty,
            payload: p.to_vec(),
        });
        if releases {
            self.release_hold();
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if self.reset_held {
            sysfs::write_file(&self.reset_line, "0"); // never leave the radio held in reset
        }
    }
}

/// A zbhci frame around `payload`.
pub(crate) fn build_frame(ty: u16, payload: &[u8]) -> Vec<u8> {
    let [t0, t1] = ty.to_be_bytes();
    // The length field is 16 bits; payloads never come near it (the radio takes 0xF9).
    let [n0, n1] = (payload.len() as u16).to_be_bytes();
    let mut f = Vec::with_capacity(payload.len() + 7);
    f.extend_from_slice(&[0x55, t0, t1, n0, n1]);
    f.push(payload.iter().fold(t0 ^ t1 ^ n0 ^ n1, |x, b| x ^ b));
    f.extend_from_slice(payload);
    f.push(0xAA);
    f
}

/// Finds the frames in `rx`, calling `on_frame` for each; returns how many bytes are used up
/// (frames and garbage before them; an incomplete frame at the end stays).
pub(crate) fn scan_frames(rx: &[u8], mut on_frame: impl FnMut(u16, &[u8])) -> usize {
    let mut pos = 0;
    while rx.len() - pos >= 7 {
        if rx[pos] != 0x55 {
            pos += 1;
            continue;
        }
        let h = &rx[pos..];
        let ty = u16::from_be_bytes([h[1], h[2]]);
        let len = usize::from(u16::from_be_bytes([h[3], h[4]]));
        // Normal frames carry at most 0xF9 bytes; only the crash report is longer. A stray 0x55
        // with a bogus length must not hold back the frames behind it.
        if len
            > if ty == CRASH_IND {
                MAX_PAYLOAD
            } else {
                MAX_FRAME
            }
        {
            pos += 1;
            continue;
        }
        if h.len() < len + 7 {
            break;
        }
        let payload = &h[6..6 + len];
        let x = payload.iter().fold(h[1] ^ h[2] ^ h[3] ^ h[4], |x, b| x ^ b);
        if h[6 + len] != 0xAA || x != h[5] {
            pos += 1;
            continue;
        }
        pos += len + 7;
        on_frame(ty, payload);
    }
    pos
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::advance;

    use super::*;
    use crate::test_util::{Lines, Radio, hex, link_pair, poll_once};

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| seed.wrapping_add((i * 13) as u8)).collect()
    }

    fn frame(ty: u16, p: &[u8]) -> Vec<u8> {
        build_frame(ty, p)
    }

    /// The C++ scanner's output on the same streams (tests/golden/link_frames.txt).
    #[test]
    fn frames_match_cxx_golden() {
        let mut out = Vec::new();
        for (ty, p) in [
            (0x0A00, vec![]),
            (0x0900, vec![]),
            (0x0A03, vec![0x3C]),
            (0x0A10, pattern(16, 1)),
            (0x0A04, pattern(0xF9, 7)),
            (0x1234, pattern(300, 9)),
        ] {
            out.push(format!("build {ty:04x} {}", hex(&build_frame(ty, &p))));
        }
        let mut case = |name: &str, chunks: Vec<Vec<u8>>| {
            out.push(format!("case {name}"));
            let mut rx = Vec::new();
            for c in chunks {
                out.push(format!("in {}", c.len()));
                rx.extend_from_slice(&c);
                let used = scan_frames(&rx, |ty, p| match ty {
                    BOOT_IND => out.push("boot".into()),
                    ACK => {} // nothing in flight: ignored
                    _ => out.push(format!("frame {ty:04x} {} {}", p.len(), hex(p))),
                });
                rx.drain(..used);
                out.push(format!("rest {}", rx.len()));
            }
        };
        let cat = |parts: &[&[u8]]| parts.concat();
        case("clean", vec![frame(0x8A00, &[1, 2, 3])]);
        case("empty-payload", vec![frame(0x8A20, &[])]);
        case(
            "garbage",
            vec![cat(&[
                &[0x00, 0x12, 0xAA, 0xFF],
                &frame(0x8A10, &[0x05, 0x00]),
                &[0x13, 0x37],
                &frame(0x8A11, &pattern(20, 3)),
                &[0x01],
            ])],
        );
        let mut bad = frame(0x8A10, &[0x01, 0x02]);
        bad[5] ^= 0x40;
        case("bad-xor", vec![cat(&[&bad, &frame(0x8A10, &[0x03, 0x04])])]);
        let mut bad = frame(0x8A10, &[0x01, 0x02]);
        *bad.last_mut().unwrap() = 0xAB;
        case("bad-end", vec![cat(&[&bad, &frame(0x8A10, &[0x05, 0x06])])]);
        case(
            "stray-start",
            vec![cat(&[
                &[0x55, 0x8A, 0x11, 0xFF, 0xFF, 0x00, 0x00],
                &frame(0x8A00, &[0x07]),
            ])],
        );
        case(
            "stray-start-plausible-length",
            vec![
                cat(&[
                    &[0x55, 0x8A, 0x11, 0x00, 0x40, 0x00],
                    &frame(0x8A00, &[0x08]),
                ]),
                pattern(0x40, 1),
            ],
        );
        case("crash-report", vec![frame(0x8909, &pattern(0x1000, 5))]);
        case("crash-report-max", vec![frame(0x8909, &pattern(0x1100, 6))]);
        case(
            "crash-report-too-long",
            vec![cat(&[
                &frame(0x8909, &pattern(0x1101, 6)),
                &frame(0x8A00, &[0x09]),
            ])],
        );
        case(
            "normal-too-long",
            vec![cat(&[
                &frame(0x8A11, &pattern(0xFA, 2)),
                &frame(0x8A00, &[0x0A]),
            ])],
        );
        case("normal-max", vec![frame(0x8A11, &pattern(0xF9, 4))]);
        let f = frame(0x8A11, &pattern(30, 8));
        case(
            "split",
            vec![f[..3].to_vec(), f[3..20].to_vec(), f[20..].to_vec()],
        );
        let mut outer = frame(0x8A11, &cat(&[&[0xEE], &frame(0x8A00, &[0x0B, 0x0C])]));
        outer[5] ^= 1;
        case("nested", vec![outer]);
        case(
            "boot",
            vec![cat(&[&frame(0x8907, &[]), &frame(0x8A00, &[0x0D])])],
        );
        case(
            "ack-idle",
            vec![cat(&[
                &frame(0x8000, &[0x0A, 0x00, 0x00, 0x00]),
                &frame(0x8A00, &[0x0E]),
            ])],
        );
        crate::test_util::assert_golden(
            include_str!("../tests/golden/link_frames.txt"),
            &out.join("\n"),
        );
    }

    /// Runs the link until it waits for input or time.
    fn settle(link: &mut Link) -> Vec<LinkEvent> {
        let mut evs = Vec::new();
        while let Some(ev) = poll_once(link.next()) {
            evs.push(ev);
        }
        evs
    }

    async fn ms(link: &mut Link, n: u64) -> Vec<LinkEvent> {
        advance(Duration::from_millis(n)).await;
        settle(link)
    }

    /// A link whose application has started (reset lines driven, boot indication answered).
    async fn booted() -> (Link, Radio, Lines) {
        let lines = Lines::new();
        let (mut link, mut radio) = link_pair(&lines.radio_lines());
        assert_eq!(lines.get(), ("0".into(), "1".into()));
        radio.send(BOOT_IND, &[]); // predates the pulse's end: ignored
        assert!(ms(&mut link, 199).await.is_empty());
        assert!(ms(&mut link, 1).await.is_empty());
        assert_eq!(lines.get(), ("0".into(), "0".into()));
        radio.send(BOOT_IND, &[]);
        assert_eq!(settle(&mut link), [LinkEvent::Boot]);
        (link, radio, lines)
    }

    fn ack(tag: AckTag, status: i32) -> LinkEvent {
        LinkEvent::Ack { tag, status }
    }

    #[tokio::test(start_paused = true)]
    async fn reset_waits_for_boot_then_sends() {
        let lines = Lines::new();
        let (mut link, mut radio) = link_pair(&lines.radio_lines());
        link.send(0x0900, &[], Some(AckTag::Form));
        assert!(settle(&mut link).is_empty());
        assert_eq!(
            radio.read_frame(),
            None,
            "nothing goes out during the reset"
        );
        assert!(ms(&mut link, 200).await.is_empty());
        // No boot indication: after 3 s the link carries on.
        assert!(ms(&mut link, 2999).await.is_empty());
        assert_eq!(ms(&mut link, 1).await, [LinkEvent::Boot]);
        assert_eq!(radio.read_frame(), Some((0x0900, vec![])));
        radio.ack(0x0900, 0);
        assert_eq!(settle(&mut link), [ack(AckTag::Form, 0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn undrivable_lines_boot_at_once() {
        let (mut link, mut radio) = link_pair(&crate::test_util::absent_lines());
        link.send(0x0045, &[], None);
        assert_eq!(settle(&mut link), [LinkEvent::Boot]);
        assert_eq!(radio.read_frame(), Some((0x0045, vec![])));
    }

    #[tokio::test(start_paused = true)]
    async fn stop_and_wait_with_ack_matching() {
        let (mut link, mut radio, _lines) = booted().await;
        link.send(0x0A00, &[1], Some(AckTag::Form));
        link.send(0x0A03, &[2], Some(AckTag::Steer));
        link.send(0x0A05, &[3], Some(AckTag::Leave));
        assert_eq!(radio.read_frame(), Some((0x0A00, vec![1])));
        assert_eq!(radio.read_frame(), None, "second frame waits for the ACK");
        radio.ack(0x0A99, 0); // another type: not this frame's
        radio.send(ACK, &[0x0A, 0x00]); // too short
        assert!(settle(&mut link).is_empty());
        assert_eq!(radio.read_frame(), None);
        radio.ack(0x0A00, 0);
        assert_eq!(settle(&mut link), [ack(AckTag::Form, 0)]);
        assert_eq!(radio.read_frame(), Some((0x0A03, vec![2])));
        radio.ack(0xFFFF, 2); // concerns the frame in flight
        assert_eq!(settle(&mut link), [ack(AckTag::Steer, 2)]);
        assert_eq!(radio.read_frame(), Some((0x0A05, vec![3])));
        radio.ack(0x0A05, 0);
        radio.ack(0x0A05, 0); // nothing in flight any more
        assert_eq!(settle(&mut link), [ack(AckTag::Leave, 0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_then_gives_up_then_resets() {
        let (mut link, mut radio, lines) = booted().await;
        // Two frames without ACK: three tries each, -1, failures counted.
        for (i, ty) in [0x0A01u16, 0x0A02].into_iter().enumerate() {
            link.send(ty, &[], Some(AckTag::Data(i as u8)));
            for _ in 0..2 {
                assert_eq!(radio.read_frame(), Some((ty, vec![])));
                assert!(ms(&mut link, 299).await.is_empty());
                assert_eq!(radio.read_frame(), None);
                assert!(ms(&mut link, 1).await.is_empty());
            }
            assert_eq!(radio.read_frame(), Some((ty, vec![])));
            assert_eq!(ms(&mut link, 300).await, [ack(AckTag::Data(i as u8), -1)]);
        }
        // The third in a row resets the radio; what was queued fails with -1.
        link.send(0x0A03, &[], Some(AckTag::Form));
        link.send(0x0A04, &[], Some(AckTag::EdScan));
        link.send(0x0A05, &[], None);
        for _ in 0..2 {
            assert_eq!(radio.read_frame(), Some((0x0A03, vec![])));
            assert!(ms(&mut link, 300).await.is_empty());
        }
        assert_eq!(radio.read_frame(), Some((0x0A03, vec![])));
        assert_eq!(
            ms(&mut link, 300).await,
            [ack(AckTag::Form, -1), ack(AckTag::EdScan, -1)]
        );
        assert_eq!(lines.get(), ("0".into(), "1".into()));
        assert_eq!(radio.read_frame(), None);
        // Frames sent during the reset wait for the boot.
        link.send(0x0A06, &[], Some(AckTag::Steer));
        assert!(ms(&mut link, 200).await.is_empty());
        assert_eq!(lines.get().1, "0");
        radio.send(BOOT_IND, &[]);
        assert_eq!(settle(&mut link), [LinkEvent::Boot]);
        assert_eq!(radio.read_frame(), Some((0x0A06, vec![])));
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_clears_the_failure_count() {
        let (mut link, mut radio, lines) = booted().await;
        for round in 0..3 {
            for ty in [0x0A01u16, 0x0A02] {
                link.send(ty, &[], None);
                for _ in 0..3 {
                    assert_eq!(radio.read_frame(), Some((ty, vec![])));
                    ms(&mut link, 300).await;
                }
            }
            link.send(0x0A03, &[], None);
            assert_eq!(radio.read_frame(), Some((0x0A03, vec![])), "round {round}");
            radio.ack(0x0A03, 7); // a rejection still is an answer
            settle(&mut link);
        }
        assert_eq!(lines.get().1, "0", "never reset");
    }

    #[tokio::test(start_paused = true)]
    async fn hold_until_awaited_frame_or_timeout() {
        let (mut link, mut radio, _lines) = booted().await;
        link.send_and_hold(0x0A06, &[9], Some(AckTag::EdScan), 0x8A06, 1000);
        link.send(0x0A10, &[1], None);
        assert_eq!(radio.read_frame(), Some((0x0A06, vec![9])));
        radio.ack(0x0A06, 0);
        assert_eq!(settle(&mut link), [ack(AckTag::EdScan, 0)]);
        radio.send(0x8A11, &[5]); // other frames pass, the hold stays
        assert_eq!(
            settle(&mut link),
            [LinkEvent::Frame {
                ty: 0x8A11,
                payload: vec![5]
            }]
        );
        assert_eq!(radio.read_frame(), None);
        radio.send(0x8A06, &[0]);
        assert_eq!(
            settle(&mut link),
            [LinkEvent::Frame {
                ty: 0x8A06,
                payload: vec![0]
            }]
        );
        assert_eq!(radio.read_frame(), Some((0x0A10, vec![1])));
        radio.ack(0x0A10, 0);
        settle(&mut link);

        // Timed out: released after hold_ms.
        link.send_and_hold(0x0A06, &[], None, 0x8A06, 1000);
        link.send(0x0A10, &[2], None);
        assert_eq!(radio.read_frame(), Some((0x0A06, vec![])));
        radio.ack(0x0A06, 0);
        settle(&mut link);
        assert!(ms(&mut link, 999).await.is_empty());
        assert_eq!(radio.read_frame(), None);
        ms(&mut link, 1).await;
        assert_eq!(radio.read_frame(), Some((0x0A10, vec![2])));
        radio.ack(0x0A10, 0);
        settle(&mut link);

        // A refused command does not hold.
        link.send_and_hold(0x0A06, &[], None, 0x8A06, 1000);
        link.send(0x0A10, &[3], None);
        assert_eq!(radio.read_frame(), Some((0x0A06, vec![])));
        radio.ack(0x0A06, 2);
        settle(&mut link);
        assert_eq!(radio.read_frame(), Some((0x0A10, vec![3])));
    }

    #[tokio::test(start_paused = true)]
    async fn spontaneous_boot_resends_what_was_in_flight() {
        let (mut link, mut radio, lines) = booted().await;
        link.send_and_hold(0x0A06, &[], None, 0x8A06, 10_000);
        link.send(0x0A10, &[1], Some(AckTag::Data(1)));
        assert_eq!(radio.read_frame(), Some((0x0A06, vec![])));
        radio.ack(0x0A06, 0);
        settle(&mut link);
        // Held; the radio restarts: the hold is gone, the queue goes out again.
        radio.send(BOOT_IND, &[]);
        assert_eq!(settle(&mut link), [LinkEvent::Boot]);
        assert_eq!(radio.read_frame(), Some((0x0A10, vec![1])));
        // In flight when the radio restarts: sent again with a fresh set of tries.
        ms(&mut link, 299).await;
        radio.send(BOOT_IND, &[]);
        assert_eq!(settle(&mut link), [LinkEvent::Boot]);
        assert_eq!(radio.read_frame(), Some((0x0A10, vec![1])));
        for _ in 0..2 {
            assert!(ms(&mut link, 300).await.is_empty());
            assert_eq!(radio.read_frame(), Some((0x0A10, vec![1])));
        }
        assert_eq!(ms(&mut link, 300).await, [ack(AckTag::Data(1), -1)]);
        assert_eq!(lines.get().1, "0");
    }

    #[tokio::test(start_paused = true)]
    async fn crash_report_and_frames_pass_through() {
        let (mut link, mut radio, _lines) = booted().await;
        let report = pattern(0x1000, 3);
        radio.write_raw(&[0x00, 0x55]);
        radio.send(CRASH_IND, &report);
        radio.send(0x8A11, &[]);
        assert_eq!(
            settle(&mut link),
            [
                LinkEvent::Frame {
                    ty: CRASH_IND,
                    payload: report
                },
                LinkEvent::Frame {
                    ty: 0x8A11,
                    payload: vec![]
                }
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drop_releases_a_held_reset() {
        let lines = Lines::new();
        let (link, _radio) = link_pair(&lines.radio_lines());
        assert_eq!(lines.get().1, "1");
        drop(link);
        assert_eq!(lines.get().1, "0");
    }

    #[test]
    fn open_reports_a_missing_tty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let lines = RadioLines {
                tty: "/nonexistent/ttyS2".into(),
                reset: String::new(),
                boot: String::new(),
            };
            assert!(Link::open(&lines).is_none());
        });
    }

    /// A tty whose other end is gone (pty master closed: hangup, EIO) must not spin: reading
    /// stops, timers go on (the ACK times out), then next() just waits. Run on its own thread
    /// so a regression fails on the deadline instead of hanging the test run.
    #[test]
    fn hangup_stops_reading_without_spinning() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            rt.block_on(async {
                let (master, slave) = sys::open_pty().unwrap();
                let lines = RadioLines {
                    tty: slave,
                    ..crate::test_util::absent_lines()
                };
                let mut link = Link::open(&lines).unwrap();
                assert_eq!(link.next().await, LinkEvent::Boot);
                drop(master);
                link.send(0x0900, &[], Some(AckTag::Form));
                let ev = tokio::time::timeout(Duration::from_secs(5), link.next()).await;
                assert_eq!(ev.ok(), Some(ack(AckTag::Form, -1)));
                assert!(link.read_dead);
                let idle = tokio::time::timeout(Duration::from_secs(3600), link.next()).await;
                assert!(idle.is_err(), "nothing left to report");
            });
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("link spun on a hung-up tty");
    }

    async fn next_within_2s(link: &mut Link) -> LinkEvent {
        tokio::time::timeout(Duration::from_secs(2), link.next())
            .await
            .expect("no event from the link")
    }

    /// Reads a frame the link wrote to the pty, waiting up to 2 s.
    fn read_pty_frame(master: &mut File) -> (u16, Vec<u8>) {
        let mut buf = Vec::new();
        loop {
            assert!(sys::wait_readable(master, 2000), "no frame from the link");
            let mut b = [0u8; 64];
            let n = master.read(&mut b).unwrap();
            buf.extend_from_slice(&b[..n]);
            if buf.len() >= 7 && buf.len() >= 7 + usize::from(u16::from_be_bytes([buf[3], buf[4]]))
            {
                let ty = u16::from_be_bytes([buf[1], buf[2]]);
                let p = buf[6..buf.len() - 1].to_vec();
                assert_eq!(build_frame(ty, &p), buf);
                return (ty, p);
            }
        }
    }

    /// A real tty through Link::open: an empty non-blocking read with VMIN = VTIME = 0 returns
    /// 0 bytes, which must not count as a hangup. Input arriving in separate chunks keeps being
    /// read; only closing the pty's master ends reading, once, without spinning.
    #[test]
    fn tty_keeps_reading_across_chunks_then_detects_hangup() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let (mut master, slave) = sys::open_pty().unwrap();
                let lines = RadioLines {
                    tty: slave,
                    ..crate::test_util::absent_lines()
                };
                let mut link = Link::open(&lines).unwrap();
                let pause = || tokio::time::sleep(Duration::from_millis(50));
                assert_eq!(next_within_2s(&mut link).await, LinkEvent::Boot);
                link.send(0x0900, &[], Some(AckTag::Form));
                assert_eq!(read_pty_frame(&mut master), (0x0900, vec![]));
                pause().await;
                master.write_all(&build_frame(BOOT_IND, &[])).unwrap();
                assert_eq!(next_within_2s(&mut link).await, LinkEvent::Boot);
                // Restarted: what was in flight goes out again.
                assert_eq!(read_pty_frame(&mut master), (0x0900, vec![]));
                pause().await;
                let mut chunk = build_frame(ACK, &[0x09, 0x00, 0x00, 0x00]);
                chunk.extend(build_frame(0x8900, &[0x50, 0, 0, 1]));
                master.write_all(&chunk).unwrap();
                assert_eq!(next_within_2s(&mut link).await, ack(AckTag::Form, 0));
                assert_eq!(
                    next_within_2s(&mut link).await,
                    LinkEvent::Frame {
                        ty: 0x8900,
                        payload: vec![0x50, 0, 0, 1]
                    }
                );
                pause().await;
                master.write_all(&build_frame(0x8A00, &[7])).unwrap();
                assert_eq!(
                    next_within_2s(&mut link).await,
                    LinkEvent::Frame {
                        ty: 0x8A00,
                        payload: vec![7]
                    }
                );
                assert!(!link.read_dead);

                drop(master);
                link.send(0x0005, &[], Some(AckTag::Leave));
                assert_eq!(next_within_2s(&mut link).await, ack(AckTag::Leave, -1));
                assert!(link.read_dead);
                let idle = tokio::time::timeout(Duration::from_millis(500), link.next()).await;
                assert!(idle.is_err(), "nothing left to report");
            });
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("link test failed or spun");
    }
}
