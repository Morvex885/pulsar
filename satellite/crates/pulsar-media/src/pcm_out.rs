//! ANNOUNCE output: the channel's playbin decodes into an fdsink writing one end of a
//! socketpair, a reader thread per item moves that PCM into a bounded feed, and one writer thread
//! plays it on the model's playback PCM (`Model::playback_pcm`, the dmix the other outputs share).
//!
//! Why not alsasink (GStreamer 1.14.5, measured on a Midi with the speaker loopback): HA streams
//! an answer at about the speaking rate; queue2 drops its first buffering message (uridecodebin
//! sets use-buffering before the queue has a parent), so playback started at once, then queue2
//! reported < 100 % ~0.25 s in and the channel paused for ~1.8 s (until playsink's 1 s queue and
//! queue2 had filled). alsasink has no pause: PLAYING -> PAUSED is snd_pcm_drop, ~200 ms of the
//! answer were lost each time. Without the pauses a clock-synced sink drops whatever arrives late.
//! Here every sample plays in order: a stream that stalls is silent for as long as it stalls,
//! nothing is skipped, and Done waits until the last sample has been played.
//!
//! Why fdsink: the Midi's libquasar_gstreamer.so exports neither the appsink API nor
//! gst_buffer_map; parse_launch, g_object_get / g_object_set and gst_bin_get_by_name are there on
//! every model.

use std::collections::VecDeque;
use std::io::Read as _;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use pulsar_alsa::{Format, HwRequest, Pcm, Stream};

/// The format playsink converts to (capsfilter before the fdsink) and the PCM is opened with.
pub(crate) const CAPS: &str = "audio/x-raw,format=S16LE,layout=interleaved,rate=48000,channels=2";
const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const FRAME_BYTES: usize = 4;
const fn ms_bytes(ms: usize) -> usize {
    RATE as usize * FRAME_BYTES * ms / 1000
}
/// Output starts (and starts again after a stall) once this much is queued, or at the end of the
/// item: 150 ms late chunks of a stream paced at the speaking rate then play without a gap.
const PRIME_BYTES: usize = ms_bytes(200);
/// A stall: no PCM queued and the device holds less than this many periods. The writer then
/// keeps the device fed with silence until it primed again: a dmix that runs dry and restarts
/// lost ~60 ms of the samples written next (Midi, measured).
const LOW_PERIODS: usize = 2;
/// The reader stops taking from the socket above this, so decoding waits for the output.
const FEED_MAX_BYTES: usize = ms_bytes(1000);
/// Silence written after the last sample; the item has played out once snd_pcm_delay() reports
/// the pad played too. On the Midi the delay reads ~70 ms short of what is still to be heard, so
/// closing at the last sample's delay cut the end of every answer.
const PAD_FRAMES: usize = RATE as usize * 100 / 1000;
/// Poll period of the writer while an item is active.
const SLICE: Duration = Duration::from_millis(10);
/// Added to the PCM buffer duration as the longest wait for the play-out after the last sample.
const DRAIN_SLACK: Duration = Duration::from_millis(500);

/// Writer progress of an output item, reported on the GLib thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OutEvent {
    /// The first samples went to the device.
    Started(u64),
    /// The last sample has been played.
    PlayedOut(u64),
    /// The PCM could not be opened or failed for good.
    Failed(u64),
}

#[derive(Default)]
struct FeedState {
    data: VecDeque<u8>,
    eof: bool,
    cancelled: bool,
}

/// PCM of one item between its reader and the writer.
#[derive(Default)]
struct Feed {
    state: Mutex<FeedState>,
    space: Condvar,
}

impl Feed {
    fn lock(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn cancel(&self) {
        let mut st = self.lock();
        st.cancelled = true;
        st.data.clear();
        drop(st);
        self.space.notify_all();
    }
}

enum Cmd {
    Start(u64, Arc<Feed>),
    Flush,
    Quit,
}

struct Item {
    /// The end the fdsink writes; dropping it is the item's end of stream for the reader.
    #[allow(dead_code, reason = "held for its drop only")]
    sink_end: UnixStream,
    feed: Arc<Feed>,
}

/// Owned by the GLib thread's backend.
pub(crate) struct Output {
    cmds: Sender<Cmd>,
    /// f32 bits of the channel gain the writer applies (playsink's volume would act on PCM that
    /// is already queued, up to ~2 s late).
    gain: Arc<AtomicU32>,
    item: Option<Item>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    /// Starts the writer thread on PCM `device`; `notify` runs on that thread.
    pub(crate) fn spawn(device: &str, notify: Box<dyn Fn(OutEvent) + Send>) -> Option<Output> {
        let (cmds, rx) = mpsc::channel();
        let gain = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let w = Writer {
            device: device.to_owned(),
            notify,
            gain: Arc::clone(&gain),
            scratch: Vec::new(),
            low_frames: 0,
            cur: None,
            pcm: None,
            phase: Phase::Priming,
            started: false,
            drain_deadline: None,
            pad_left: 0,
            buffer_frames: 0,
            period_frames: 0,
            silence: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("announce-out".into())
            .spawn(move || w.run(&rx))
            .inspect_err(|e| error!("player: announce output thread: {e}"))
            .ok()?;
        Some(Output {
            cmds,
            gain,
            item: None,
            thread: Some(thread),
        })
    }

    /// Item `item` starts (any previous one is dropped); returns the fd for the fdsink.
    pub(crate) fn begin(&mut self, item: u64) -> Option<RawFd> {
        self.flush();
        let (src, sink_end) = UnixStream::pair()
            .inspect_err(|e| error!("player: announce socketpair: {e}"))
            .ok()?;
        // Non-blocking: a full socket makes the fdsink wait in its GstPoll, which a state change
        // to NULL interrupts, instead of in write().
        sink_end
            .set_nonblocking(true)
            .inspect_err(|e| error!("player: announce socket: {e}"))
            .ok()?;
        let feed = Arc::new(Feed::default());
        let reader_feed = Arc::clone(&feed);
        std::thread::Builder::new()
            .name("announce-rd".into())
            .spawn(move || read_feed(src, &reader_feed))
            .inspect_err(|e| error!("player: announce reader thread: {e}"))
            .ok()?;
        let _ = self.cmds.send(Cmd::Start(item, Arc::clone(&feed)));
        let fd = sink_end.as_raw_fd();
        self.item = Some(Item { sink_end, feed });
        Some(fd)
    }

    /// The pipeline reached EOS, everything decoded is in the socket: the reader sees its end
    /// once it has read the rest, and the writer reports PlayedOut after playing it.
    pub(crate) fn end(&mut self) {
        self.item = None;
    }

    /// Channel gain 0..1, applied to what is written from now on.
    pub(crate) fn set_gain(&self, gain: f64) {
        #[allow(clippy::cast_possible_truncation, reason = "a gain 0..1")]
        self.gain.store((gain as f32).to_bits(), Ordering::Relaxed);
    }

    /// Drops the current item: queued PCM is discarded and the device stops at once.
    pub(crate) fn flush(&mut self) {
        if let Some(item) = self.item.take() {
            item.feed.cancel();
        }
        let _ = self.cmds.send(Cmd::Flush);
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.flush();
        let _ = self.cmds.send(Cmd::Quit);
        if let Some(t) = self.thread.take()
            && t.join().is_err()
        {
            error!("player: announce output thread panicked");
        }
    }
}

/// Socket -> feed until the fdsink's end is closed; discards once the item is cancelled.
fn read_feed(mut src: UnixStream, feed: &Feed) {
    let mut chunk = [0u8; 4096];
    loop {
        let n = match src.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                warn!("player: announce read: {e}");
                break;
            }
        };
        let mut st = feed.lock();
        while st.data.len() >= FEED_MAX_BYTES && !st.cancelled {
            st = feed.space.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        if !st.cancelled {
            st.data.extend(&chunk[..n]);
        }
    }
    feed.lock().eof = true;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for PRIME_BYTES (or the end of the item) before writing.
    Priming,
    Playing,
    /// Everything is written (plus the silence pad); waiting for the device to play it.
    Draining,
}

struct Writer {
    device: String,
    notify: Box<dyn Fn(OutEvent) + Send>,
    gain: Arc<AtomicU32>,
    /// Gain-scaled PCM on its way to the device.
    scratch: Vec<u8>,
    /// Device fill below which an empty feed is a stall.
    low_frames: usize,
    cur: Option<(u64, Arc<Feed>)>,
    pcm: Option<Pcm>,
    phase: Phase,
    /// Started was reported for the current item.
    started: bool,
    drain_deadline: Option<Instant>,
    /// Frames of the end pad still to write.
    pad_left: usize,
    /// PCM buffer and period, frames.
    buffer_frames: usize,
    period_frames: usize,
    /// One period of silence.
    silence: Vec<u8>,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Cmd>) {
        loop {
            let cmd = if self.cur.is_some() {
                match rx.recv_timeout(SLICE) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                }
            };
            if let Some(mut c) = cmd {
                loop {
                    match c {
                        Cmd::Start(item, feed) => {
                            self.reset();
                            self.cur = Some((item, feed));
                        }
                        Cmd::Flush => self.reset(),
                        Cmd::Quit => {
                            self.reset();
                            return;
                        }
                    }
                    match rx.try_recv() {
                        Ok(next) => c = next,
                        Err(_) => break,
                    }
                }
            }
            self.service();
        }
        self.reset();
    }

    /// Drops the current item and stops the device at once.
    fn reset(&mut self) {
        // A reader still waiting for space (the item was replaced after EOS, with the feed full)
        // must not wait forever.
        if let Some((_, feed)) = self.cur.take() {
            feed.cancel();
        }
        self.phase = Phase::Priming;
        self.started = false;
        self.drain_deadline = None;
        self.pad_left = 0;
        if let Some(mut pcm) = self.pcm.take() {
            let _ = pcm.drop_frames();
            debug!("player: announce output closed");
        }
    }

    fn finish(&mut self, ev: OutEvent) {
        self.reset();
        (self.notify)(ev);
    }

    fn service(&mut self) {
        let Some((item, feed)) = self.cur.as_ref().map(|(g, f)| (*g, Arc::clone(f))) else {
            return;
        };
        let mut st = feed.lock();
        if self.phase == Phase::Priming {
            let len = st.data.len();
            if len >= PRIME_BYTES || (st.eof && len >= FRAME_BYTES) {
                if self.pcm.is_none() {
                    // Not under the feed lock: the GLib thread takes it to cancel the item.
                    drop(st);
                    match self.open() {
                        Ok(pcm) => self.pcm = Some(pcm),
                        Err((step, e)) => {
                            error!(
                                "player: announce output '{}' {step} failed: {}",
                                self.device,
                                pulsar_alsa::strerror(e)
                            );
                            self.finish(OutEvent::Failed(item));
                            return;
                        }
                    }
                    st = feed.lock();
                }
                self.phase = Phase::Playing;
                if !self.started {
                    self.started = true;
                    (self.notify)(OutEvent::Started(item));
                }
            } else if st.eof {
                // Nothing left to write: play out what the device holds, if it is open.
                drop(st);
                if self.pcm.is_some() {
                    self.start_drain();
                } else {
                    self.finish(OutEvent::PlayedOut(item));
                }
                return;
            } else {
                drop(st);
                if self.keep_alive().is_err() {
                    self.finish(OutEvent::Failed(item));
                }
                return;
            }
        }
        if self.phase == Phase::Playing {
            let wrote = match self.write(&mut st) {
                Ok(w) => w,
                Err(()) => {
                    drop(st);
                    self.finish(OutEvent::Failed(item));
                    return;
                }
            };
            let (done, empty) = (
                st.eof && st.data.len() < FRAME_BYTES,
                st.data.len() < FRAME_BYTES,
            );
            drop(st);
            if wrote {
                feed.space.notify_all();
            }
            if done {
                self.start_drain();
            } else if empty && self.fill().is_some_and(|f| f < self.low_frames) {
                info!("player: announce output stalled, priming again");
                self.phase = Phase::Priming;
                if self.keep_alive().is_err() {
                    self.finish(OutEvent::Failed(item));
                }
            }
            return;
        }
        drop(st);
        self.drain(item);
    }

    /// Frames queued in the device; None when that cannot be read.
    fn fill(&mut self) -> Option<usize> {
        let avail = self.pcm.as_mut()?.avail_update().ok()?;
        Some(self.buffer_frames.saturating_sub(avail))
    }

    /// While stalled: tops the device up with silence so it never runs dry.
    fn keep_alive(&mut self) -> Result<(), ()> {
        let Some(pcm) = self.pcm.as_mut() else {
            return Ok(());
        };
        let avail = match pcm.avail_update() {
            Ok(n) => n,
            Err(e) => return self.recover(e),
        };
        let fill = self.buffer_frames.saturating_sub(avail);
        if fill >= self.low_frames || avail < self.period_frames {
            return Ok(());
        }
        match pcm.writei(&self.silence, self.period_frames) {
            Ok(_) => Ok(()),
            Err(e) => self.recover(e),
        }
    }

    /// Writes what the device takes. Err when the PCM failed for good.
    fn write(&mut self, st: &mut FeedState) -> Result<bool, ()> {
        let mut wrote = false;
        loop {
            let frames = st.data.len() / FRAME_BYTES;
            if frames == 0 {
                return Ok(wrote);
            }
            let Some(pcm) = self.pcm.as_mut() else {
                return Err(());
            };
            let avail = match pcm.avail_update() {
                Ok(n) => n,
                Err(e) => {
                    self.recover(e)?;
                    continue;
                }
            };
            if avail == 0 {
                return Ok(wrote);
            }
            let n = frames.min(avail);
            let gain = f32::from_bits(self.gain.load(Ordering::Relaxed));
            let (head, tail) = st.data.as_slices();
            let head_frames = head.len() / FRAME_BYTES;
            let res = if gain == 1.0 && head_frames > 0 {
                let n = n.min(head_frames);
                pcm.writei(&head[..n * FRAME_BYTES], n)
            } else {
                // Scaled (or a frame across the ring's wrap): copied into scratch.
                self.scratch.clear();
                let mut bytes = head.iter().chain(tail).copied().take(n * FRAME_BYTES);
                while let (Some(lo), Some(hi)) = (bytes.next(), bytes.next()) {
                    #[allow(clippy::cast_possible_truncation, reason = "saturating, gain 0..1")]
                    let v = (f32::from(i16::from_le_bytes([lo, hi])) * gain) as i16;
                    self.scratch.extend_from_slice(&v.to_le_bytes());
                }
                pcm.writei(&self.scratch, n)
            };
            match res {
                Ok(w) => {
                    st.data.drain(..w * FRAME_BYTES);
                    wrote = true;
                }
                Err(e) => self.recover(e)?,
            }
        }
    }

    fn start_drain(&mut self) {
        self.phase = Phase::Draining;
        let buffer = Duration::from_micros(self.buffer_frames as u64 * 1_000_000 / u64::from(RATE));
        self.drain_deadline = Some(Instant::now() + buffer + DRAIN_SLACK);
        self.pad_left = PAD_FRAMES;
    }

    fn drain(&mut self, item: u64) {
        let Some(pcm) = self.pcm.as_mut() else {
            self.finish(OutEvent::PlayedOut(item));
            return;
        };
        while self.pad_left > 0 {
            let n = match pcm.avail_update() {
                Ok(a) => a.min(self.pad_left).min(self.period_frames),
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            match pcm.writei(&self.silence[..n * FRAME_BYTES], n) {
                Ok(w) => self.pad_left -= w,
                Err(_) => break,
            }
        }
        let played = match pcm.delay() {
            Ok(d) => self.pad_left == 0 && d <= 0,
            // Ran dry (xrun): nothing left to play.
            Err(_) => true,
        };
        if played || self.drain_deadline.is_some_and(|t| Instant::now() >= t) {
            if !played {
                warn!("player: announce output play-out timed out");
            }
            self.finish(OutEvent::PlayedOut(item));
        }
    }

    fn open(&mut self) -> Result<Pcm, (&'static str, i32)> {
        let mut pcm = Pcm::open(&self.device, Stream::Playback, false).map_err(|e| ("open", e))?;
        let hw = pcm
            .set_hw_params(&HwRequest {
                format: Format::S16Le,
                channels: CHANNELS,
                rate: RATE,
                rate_exact: true,
                period_frames: None,
                buffer_frames: None,
            })
            .map_err(|e| ("hw_params", e))?;
        let period = hw.period_frames.max(1);
        pcm.set_sw_params(period, period)
            .map_err(|e| ("sw_params", e))?;
        pcm.prepare().map_err(|e| ("prepare", e))?;
        self.buffer_frames = usize::try_from(hw.buffer_frames).unwrap_or(usize::MAX);
        self.period_frames = usize::try_from(period).unwrap_or(usize::MAX);
        self.silence.resize(self.period_frames * FRAME_BYTES, 0);
        // Never more than half the buffer: a fuller device is not stalling.
        self.low_frames = (LOW_PERIODS * self.period_frames).min(self.buffer_frames / 2);
        debug!(
            "player: announce output '{}' open (period {period}, buffer {} frames)",
            self.device, hw.buffer_frames
        );
        Ok(pcm)
    }

    /// Ok when the PCM can be written again (after an underrun or a suspend).
    fn recover(&mut self, err: i32) -> Result<(), ()> {
        if err == -libc::EINTR || err == -libc::EAGAIN {
            return Ok(());
        }
        let Some(pcm) = self.pcm.as_mut() else {
            return Err(());
        };
        let mut err = err;
        if err == -libc::EPIPE || err == -libc::ESTRPIPE {
            let mut r = if err == -libc::ESTRPIPE {
                pcm.resume()
            } else {
                Err(-1)
            };
            if r.is_err() {
                r = pcm.prepare();
            }
            match r {
                Ok(()) => return Ok(()),
                Err(e) => err = e,
            }
        }
        error!(
            "player: announce output error {}",
            pulsar_alsa::strerror(err)
        );
        Err(())
    }
}
