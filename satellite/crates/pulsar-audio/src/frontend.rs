//! Microphone front-end: ALSA capture (mic + speaker-feedback channels, layout per Model) ->
//! Yandex VQE (AEC / beamforming / noise reduction, per-model preset) -> Yandex phrase spotters
//! (stock models from /system/vendor/quasar/spotter_models): the wake word ("Алиса"/"Яндекс")
//! and, optionally, the command model ("стоп", "громче", ...).
//!
//! Two threads: a capture thread that only drains ALSA into a FIFO, and a processing thread
//! (VQE + spotters). The Lite 2's PDM capture allows at most ~42 ms of buffer (2048 frames at
//! 8 ch S16); VQE chunks occasionally take longer than the remaining slack, which overran the
//! device buffer when reading and processing shared one thread.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{
    AtomicBool, AtomicU32, AtomicU64, Ordering::AcqRel, Ordering::Acquire, Ordering::Relaxed,
};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{error, info, warn};
use pulsar_alsa::{Format, HwRequest, HwTrace, Pcm, Stream};
use pulsar_core::MusicFrame;
use pulsar_core::model::{CaptureFormat, Model, PrerollRule};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::engine::{Engine, Stage, VQE_OMNI, VQE_OMNI_WITH_GAIN, to_s16};
use crate::spectrum::SpectrumAnalyzer;

/// Wake word level window: 1.5 s in 32 ms blocks at 16 kHz, independent of VQE chunk size.
/// Long enough to cover the phrase when the spotter confirms; the loudest blocks are the phrase.
const LEVEL_CHUNKS: usize = 47;
const LEVEL_TOP: usize = 8;
/// Samples of one level window block: 32 ms at 16 kHz.
const LEVEL_SAMPLES: usize = 512;
/// Pre-roll ring: 1.024 s at 16 kHz (32 chunks of 32 ms, 64 of 16 ms on the Mini 3 and Mini),
/// more than any wake word's confirmation delay.
const PREROLL_SAMPLES: usize = 16384;
/// End of speech for the "wake timing" log: the newest block within 20 dB (power) of the
/// loudest one in the level window.
const SPEECH_END_RATIO: f32 = 0.01;

/// ALSA buffer in VQE chunks: 256 ms with 32 ms chunks, 128 ms on the Mini 3 and Mini (16 ms);
/// the device may grant less (Lite 2: 2048 frames).
const BUFFER_CHUNKS: u64 = 8;
const READ_WAIT_MS: i32 = 200;
const REOPEN_DELAY: Duration = Duration::from_secs(2);
const CHUNK_WAIT: Duration = Duration::from_millis(500);
// The VQE and the spotters run on the processing thread; the C++ daemon ran them on a default
// glibc thread (8 MiB stack), Rust's default is 2 MiB.
const PROCESSING_STACK: usize = 8 << 20;

pub enum FrontendEvent {
    /// level_db: wake word level in dBFS on VQE_OMNI (after echo cancellation, before AGC),
    /// rounded to 0.1 dB; -100 = no data.
    Wake {
        phrase: String,
        level_db: f32,
        /// 16 kHz mono audio from the end of the wake word up to and including the confirming
        /// chunk, oldest first; empty without values for the model (`Model::wake_preroll`).
        /// The `Audio` events after it continue it without a gap until
        /// [`FrontendHandle::wake_handled`].
        preroll: Vec<i16>,
    },
    Command {
        word: String,
    },
    /// 16 kHz mono, one VQE chunk (512 samples, 256 on the Mini 3 and Mini), while streaming is on
    /// or a `Wake` is not yet handled.
    Audio(Vec<i16>),
    /// One per 32 ms FFT window while music analysis is on.
    Music(MusicFrame),
}

/// Counters for frontend_probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrontendStats {
    /// Frames read from ALSA.
    pub captured_frames: u64,
    /// VQE chunks processed.
    pub chunks: u64,
    /// Frames waiting in the FIFO for the processing thread.
    pub fifo_frames: u64,
    /// Frames dropped because processing fell more than 1 s behind.
    pub dropped_frames: u64,
}

/// Controls of the front-end. Clones share it; the threads stop when the last one is dropped.
#[derive(Clone)]
pub struct FrontendHandle {
    inner: Arc<Inner>,
}

/// Starts the front-end: dlopen, VQE for the model's preset, the wake word spotter from
/// `wake_model_dir` (e.g. .../spotter_models/activation/alisa) and, if given, the command
/// spotter (.../spotter_models/command), then the capture and processing threads. On failure
/// (logged, "microphone front-end failed to start") the handle works but no event ever comes.
/// A capture device that cannot be opened is retried every 2 s.
pub fn spawn(
    model: &'static Model,
    wake_model_dir: PathBuf,
    command_model_dir: Option<PathBuf>,
) -> (FrontendHandle, UnboundedReceiver<FrontendEvent>) {
    let (tx, rx) = unbounded_channel();
    let mut inner = Inner {
        shared: Arc::new(Shared::new()),
        pipe: None,
        threads: Vec::new(),
        frame: 0,
        _events: tx.clone(),
    };
    let command_dir = command_model_dir.filter(|d| !d.as_os_str().is_empty());
    let started = match Engine::start(model, &wake_model_dir, command_dir.as_deref()) {
        Some(eng) => {
            inner.frame = eng.frame();
            inner
                .start_threads(model, eng, tx)
                .inspect_err(|e| error!("audio threads: {e}"))
                .is_ok()
        }
        None => false,
    };
    if !started {
        inner.stop();
        inner.frame = 0;
        error!("microphone front-end failed to start");
    }
    (
        FrontendHandle {
            inner: Arc::new(inner),
        },
        rx,
    )
}

impl FrontendHandle {
    /// Switches the wake word model (applied by the processing thread before the next chunk).
    pub fn set_model(&self, dir: PathBuf) {
        *lock(&self.inner.shared.pending_model) = (!dir.as_os_str().is_empty()).then_some(dir);
    }

    /// Wake word detection on/off (e.g. software mute). On by default.
    pub fn set_detection(&self, on: bool) {
        self.inner.shared.detect.store(on, Relaxed);
    }

    /// Command spotter on/off (commands without the wake word).
    pub fn set_commands(&self, on: bool) {
        self.inner.shared.commands.store(on, Relaxed);
    }

    /// While on, cleaned 16 kHz audio comes as [`FrontendEvent::Audio`].
    pub fn set_streaming(&self, on: bool) {
        self.inner.shared.stream.store(on, Relaxed);
    }

    /// The owner handled one [`FrontendEvent::Wake`]: ends the stream hold it started. Call it
    /// after `set_streaming` for that wake word.
    pub fn wake_handled(&self) {
        let _ = self
            .inner
            .shared
            .wake_pending
            .try_update(AcqRel, Acquire, |v| v.checked_sub(1));
    }

    /// Pre-roll in [`FrontendEvent::Wake`] on/off (`Model::wake_preroll`). On by default; the
    /// stream hold after a `Wake` does not depend on it.
    pub fn set_preroll(&self, on: bool) {
        self.inner.shared.preroll.store(on, Relaxed);
    }

    /// While on, music features of the speaker reference come as [`FrontendEvent::Music`].
    pub fn set_music_analysis(&self, on: bool) {
        self.inner.shared.music.store(on, Relaxed);
    }

    /// Counters of the capture and processing threads (frontend_probe).
    pub fn stats(&self) -> FrontendStats {
        let s = &self.inner.shared;
        FrontendStats {
            captured_frames: s.captured_frames.load(Relaxed),
            chunks: s.chunks.load(Relaxed),
            fifo_frames: s.fifo_frames.load(Relaxed),
            dropped_frames: s.dropped_frames.load(Relaxed),
        }
    }

    /// Samples per [`FrontendEvent::Audio`] chunk (`Engine::frame`); 0 when the front-end did not
    /// start.
    pub fn frame(&self) -> usize {
        self.inner.frame
    }

    /// Every block read from ALSA, interleaved as captured (`capture_channels` per frame), goes
    /// to `tap` while one is set (frontend_probe --record).
    pub fn set_raw_tap(&self, tap: Option<UnboundedSender<Vec<i16>>>) {
        let s = &self.inner.shared;
        let mut t = lock(&s.tap);
        s.tap_on.store(tap.is_some(), Relaxed);
        *t = tap;
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Written by the handle, read by the threads (and the counters the other way round).
struct Shared {
    running: AtomicBool,
    // With stop_cv: wakes the capture thread's reopen delay on stop.
    stop_lock: Mutex<()>,
    stop_cv: Condvar,
    detect: AtomicBool,
    commands: AtomicBool,
    stream: AtomicBool,
    /// `Wake` events sent and not yet handled: audio keeps coming meanwhile, so nothing is lost
    /// between the confirmation and the owner's `set_streaming(true)`.
    wake_pending: AtomicU32,
    preroll: AtomicBool,
    music: AtomicBool,
    pending_model: Mutex<Option<PathBuf>>,
    tap_on: AtomicBool,
    tap: Mutex<Option<UnboundedSender<Vec<i16>>>>,
    captured_frames: AtomicU64,
    chunks: AtomicU64,
    fifo_frames: AtomicU64,
    dropped_frames: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Shared {
            running: AtomicBool::new(false),
            stop_lock: Mutex::new(()),
            stop_cv: Condvar::new(),
            detect: AtomicBool::new(true),
            commands: AtomicBool::new(false),
            stream: AtomicBool::new(false),
            wake_pending: AtomicU32::new(0),
            preroll: AtomicBool::new(true),
            music: AtomicBool::new(false),
            pending_model: Mutex::new(None),
            tap_on: AtomicBool::new(false),
            tap: Mutex::new(None),
            captured_frames: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
            fifo_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
        }
    }

    fn running(&self) -> bool {
        self.running.load(Relaxed)
    }

    /// Sleeps `d` or until stopped.
    fn pause(&self, d: Duration) {
        let g = lock(&self.stop_lock);
        let _ = self
            .stop_cv
            .wait_timeout_while(g, d, |_| self.running())
            .unwrap_or_else(PoisonError::into_inner);
    }
}

/// The FIFO between the threads with its wake-up.
struct Pipe {
    fifo: Mutex<Fifo>,
    ready: Condvar,
}

struct Inner {
    shared: Arc<Shared>,
    pipe: Option<Arc<Pipe>>,
    threads: Vec<JoinHandle<()>>,
    /// Samples per VQE output chunk; 0 without a running engine.
    frame: usize,
    // Keeps the event channel open: without a front-end, recv() waits forever instead of
    // returning None.
    _events: UnboundedSender<FrontendEvent>,
}

impl Inner {
    fn start_threads(
        &mut self,
        model: &'static Model,
        eng: Engine,
        tx: UnboundedSender<FrontendEvent>,
    ) -> std::io::Result<()> {
        let nch = model.capture_channels as usize;
        let period = eng.chunk;
        let pipe = Arc::new(Pipe {
            // 1 s of audio.
            fifo: Mutex::new(Fifo::new(nch, period, model.capture_rate as usize)),
            ready: Condvar::new(),
        });
        self.pipe = Some(pipe.clone());
        self.shared.running.store(true, Relaxed);
        let (shared, p) = (self.shared.clone(), pipe.clone());
        self.threads.push(
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || capture(model, period, &shared, &p))?,
        );
        let shared = self.shared.clone();
        self.threads.push(
            std::thread::Builder::new()
                .name("audio-process".into())
                .stack_size(PROCESSING_STACK)
                .spawn(move || processing(model, eng, &shared, &pipe, &tx))?,
        );
        Ok(())
    }

    fn stop(&mut self) {
        {
            let _g = lock(&self.shared.stop_lock);
            self.shared.running.store(false, Relaxed);
        }
        self.shared.stop_cv.notify_all();
        if let Some(pipe) = &self.pipe {
            drop(lock(&pipe.fifo));
            pipe.ready.notify_all();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Interleaved capture frames, oldest first, at most `max` samples.
pub(crate) struct Fifo {
    buf: VecDeque<i16>,
    nch: usize,
    chunk: usize,
    max: usize,
}

impl Fifo {
    /// `nch` channels per frame, chunks of `chunk_frames`, at most `max_frames` queued.
    pub(crate) fn new(nch: usize, chunk_frames: usize, max_frames: usize) -> Self {
        Fifo {
            buf: VecDeque::new(),
            nch,
            chunk: chunk_frames * nch,
            max: max_frames * nch,
        }
    }

    /// Appends whole frames; past the limit the oldest are dropped: returns how many frames.
    pub(crate) fn push(&mut self, samples: &[i16]) -> Option<usize> {
        self.buf.extend(samples);
        let over = self.buf.len().checked_sub(self.max).filter(|&n| n > 0)?;
        self.buf.drain(..over);
        Some(over / self.nch)
    }

    /// After a capture error: the chunk being assembled has a gap in it. The processing thread
    /// takes whole chunks from the front, so the partial one is the tail.
    pub(crate) fn drop_partial(&mut self) {
        let keep = self.buf.len() - self.buf.len() % self.chunk;
        self.buf.truncate(keep);
    }

    pub(crate) fn has_chunk(&self) -> bool {
        self.buf.len() >= self.chunk
    }

    /// Moves exactly one chunk into `out` (its length); false if there is not one yet.
    pub(crate) fn take_chunk(&mut self, out: &mut [i16]) -> bool {
        if !self.has_chunk() || out.len() != self.chunk {
            return false;
        }
        let (a, b) = self.buf.as_slices();
        let na = a.len().min(self.chunk);
        out[..na].copy_from_slice(&a[..na]);
        out[na..].copy_from_slice(&b[..self.chunk - na]);
        self.buf.drain(..self.chunk);
        true
    }

    pub(crate) fn frames(&self) -> usize {
        self.buf.len() / self.nch
    }
}

/// Capture buffer per sample format.
enum CaptureBuf {
    S16(Vec<i16>),
    /// Raw S32 frames and their 16-bit conversion.
    S32 {
        raw: Vec<i32>,
        out: Vec<i16>,
        shift: u32,
    },
}

impl CaptureBuf {
    fn new(format: CaptureFormat, samples: usize) -> Self {
        match format {
            CaptureFormat::S16 => Self::S16(vec![0; samples]),
            CaptureFormat::S32 { shift } => Self::S32 {
                raw: vec![0; samples],
                out: vec![0; samples],
                shift,
            },
        }
    }

    fn read(&mut self, pcm: &mut Pcm, frames: usize) -> Result<usize, i32> {
        match self {
            Self::S16(buf) => pcm.readi(buf, frames),
            Self::S32 { raw, .. } => pcm.readi_s32(raw, frames),
        }
    }

    /// The first `samples` samples read, as the VQE's 16-bit input.
    fn samples(&mut self, samples: usize) -> &[i16] {
        match self {
            Self::S16(buf) => &buf[..samples],
            Self::S32 { raw, out, shift } => {
                s32_to_s16(&raw[..samples], *shift, &mut out[..samples]);
                &out[..samples]
            }
        }
    }
}

/// S32_LE carries a right-justified 24-bit sample on the Mini: shifting out the low `shift` bits
/// keeps the sign extension and gives the VQE's 16-bit input; anything wider saturates.
fn s32_to_s16(src: &[i32], shift: u32, dst: &mut [i16]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = (s >> shift).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

fn capture(model: &'static Model, period: usize, shared: &Shared, pipe: &Pipe) {
    let nch = model.capture_channels as usize;
    let mut pcm: Option<Pcm> = None;
    let mut hw_period = period;
    let mut buf = CaptureBuf::new(model.capture_format, period * nch);
    while shared.running() {
        let Some(p) = pcm.as_mut() else {
            match open_capture(model, period) {
                Some((p, per)) => {
                    pcm = Some(p);
                    hw_period = per;
                }
                None => shared.pause(REOPEN_DELAY),
            }
            continue;
        };
        let mut r = buf.read(p, hw_period);
        if r == Err(-libc::EAGAIN) {
            match p.wait(READ_WAIT_MS) {
                // Ready, or timeout (re-checks running).
                Ok(_) => continue,
                Err(e) => r = Err(e),
            }
        }
        let n = match r {
            Ok(n) => n,
            Err(e) => {
                warn!("capture: {}, recovering", pulsar_alsa::strerror(e));
                lock(&pipe.fifo).drop_partial();
                if p.recover(e, true).is_err() {
                    pcm = None;
                }
                continue;
            }
        };
        let block = buf.samples(n * nch);
        shared.captured_frames.fetch_add(n as u64, Relaxed);
        if shared.tap_on.load(Relaxed)
            && let Some(tap) = lock(&shared.tap).as_ref()
        {
            let _ = tap.send(block.to_vec());
        }
        let dropped = {
            let mut f = lock(&pipe.fifo);
            let dropped = f.push(block);
            shared.fifo_frames.store(f.frames() as u64, Relaxed);
            dropped
        };
        pipe.ready.notify_one();
        if let Some(d) = dropped {
            shared.dropped_frames.fetch_add(d as u64, Relaxed);
            warn!("capture: processing behind, dropping {d} frames");
        }
    }
}

/// The C++ name of a failed hw_params step and its error, or the rate the device offered
/// instead of the model's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HwFail {
    Step(&'static str, i32),
    Rate(u32),
}

/// Outcome of the capture hw_params in the order the C++ daemon checked them: the rate right
/// after set_rate_near, then period, buffer, apply (the period / buffer read back afterwards were
/// not checked there). Ok = the period and buffer the device chose.
pub(crate) fn hw_outcome(t: &HwTrace, rate: u32) -> Result<(u64, u64), HwFail> {
    let cxx = |step| match step {
        "any" => "hw_params_any",
        "access" => "set_access",
        "format" => "set_format",
        "channels" => "set_channels",
        "rate" => "set_rate_near",
        "period" => "set_period_size_near",
        "buffer" => "set_buffer_size_near",
        _ => "snd_pcm_hw_params",
    };
    match t.failed {
        Some((step @ ("any" | "access" | "format" | "channels" | "rate"), e)) => {
            Err(HwFail::Step(cxx(step), e))
        }
        _ if t.near.rate != rate => Err(HwFail::Rate(t.near.rate)),
        Some((step @ ("period" | "buffer" | "apply"), e)) => Err(HwFail::Step(cxx(step), e)),
        _ => Ok((t.near.period_frames, t.near.buffer_frames)),
    }
}

/// Opens the model's capture PCM: its sample width (S16 or S32), channels, exactly its rate (a
/// silent rate mismatch would feed the VQE and spotters wrongly timed audio), period = one VQE
/// chunk, buffer = 8. The period may differ (the FIFO decouples ALSA periods from VQE chunks).
/// Returns the PCM and the frames to read at once.
fn open_capture(model: &Model, period: usize) -> Option<(Pcm, usize)> {
    let dev = model.capture_pcm;
    let format = match model.capture_format {
        CaptureFormat::S16 => Format::S16Le,
        CaptureFormat::S32 { .. } => Format::S32Le,
    };
    // Non-blocking: reads never wait inside ALSA, so a stop never hangs in snd_pcm_readi.
    let mut pcm = match Pcm::open(dev, Stream::Capture, true) {
        Ok(p) => p,
        Err(e) => {
            error!("snd_pcm_open {dev}: {}", pulsar_alsa::strerror(e));
            return None;
        }
    };
    let t = pcm.set_hw_params_traced(&HwRequest {
        format,
        channels: model.capture_channels,
        rate: model.capture_rate,
        rate_exact: false,
        period_frames: Some(period as u64),
        buffer_frames: Some(period as u64 * BUFFER_CHUNKS),
    });
    match hw_outcome(&t, model.capture_rate) {
        Ok((per, buf)) => {
            info!(
                "capture {dev}: {} Hz, {} ch, {} bits, period {per}, buffer {buf}",
                t.near.rate,
                model.capture_channels,
                model.capture_format.bits()
            );
            let per = usize::try_from(per).unwrap_or(usize::MAX);
            Some((pcm, per.min(period)))
        }
        Err(HwFail::Step(what, e)) => {
            error!("capture {dev}: {what}: {}", pulsar_alsa::strerror(e));
            None
        }
        Err(HwFail::Rate(got)) => {
            error!("capture {dev}: rate {got} Hz, need {}", model.capture_rate);
            error!("capture {dev}: rate: unexpected value");
            None
        }
    }
}

/// Capture frames -> VQE input: `mics` / `spk` frame-interleaved, scaled to [-1, 1].
pub(crate) fn deinterleave(
    input: &[i16],
    nch: usize,
    mic_ch: &[u32],
    spk_ch: &[u32],
    mics: &mut [f32],
    spk: &mut [f32],
) {
    let (nmic, nspk) = (mic_ch.len(), spk_ch.len());
    for (i, f) in input.chunks_exact(nch).enumerate() {
        for (c, &ch) in mic_ch.iter().enumerate() {
            mics[i * nmic + c] = f32::from(f[ch as usize]) * (1.0 / 32768.0);
        }
        for (c, &ch) in spk_ch.iter().enumerate() {
            spk[i * nspk + c] = f32::from(f[ch as usize]) * (1.0 / 32768.0);
        }
    }
}

/// Mean of the 8 loudest of the last 47 frames (mean squares), in dBFS clamped to -100..0 and
/// rounded to 0.1 dB: stations compare levels received as "%.1f" text, so ours must be the same
/// number the peers see, or ties could resolve differently on each side.
pub(crate) fn wake_level_db(h: &[f32; LEVEL_CHUNKS]) -> f32 {
    let mut s = *h;
    s.sort_unstable_by(|a, b| b.total_cmp(a));
    let mut sum = 0.0f32;
    for &v in &s[..LEVEL_TOP] {
        sum += v;
    }
    let db = (10.0 * (sum / LEVEL_TOP as f32 + 1e-10).log10()).clamp(-100.0, 0.0);
    (db * 10.0).round() / 10.0
}

fn mean_square(d: &[f32]) -> f32 {
    let mut sq = 0.0f64;
    for &v in d {
        // `sq += (double)d[i] * d[i]`: fused on the stations (clang -ffp-contract=on).
        sq = f64::from(v).mul_add(f64::from(v), sq);
    }
    (sq / d.len() as f64) as f32
}

/// Mean squares of the last LEVEL_CHUNKS blocks of LEVEL_SAMPLES samples, whatever the VQE
/// chunk: the Mini 3's and Mini's VQE give 16 ms chunks, the window stays 47 × 32 ms on every
/// model.
struct LevelHistory {
    values: [f32; LEVEL_CHUNKS],
    pos: usize,
    pending: [f32; LEVEL_SAMPLES],
    used: usize,
}

impl LevelHistory {
    fn new() -> Self {
        Self {
            values: [0.0; LEVEL_CHUNKS],
            pos: 0,
            pending: [0.0; LEVEL_SAMPLES],
            used: 0,
        }
    }

    fn push(&mut self, mut samples: &[f32]) {
        while !samples.is_empty() {
            let n = samples.len().min(LEVEL_SAMPLES - self.used);
            self.pending[self.used..self.used + n].copy_from_slice(&samples[..n]);
            self.used += n;
            samples = &samples[n..];
            if self.used == LEVEL_SAMPLES {
                self.values[self.pos] = mean_square(&self.pending);
                self.pos = (self.pos + 1) % LEVEL_CHUNKS;
                self.used = 0;
            }
        }
    }

    /// VQE chunks of `frame` samples from the newest sample back to the end of speech
    /// (`speech_end_chunks` counts blocks; samples of an unfinished block count too).
    fn speech_end(&self, frame: usize) -> Option<usize> {
        let newest = (self.pos + LEVEL_CHUNKS - 1) % LEVEL_CHUNKS;
        speech_end_chunks(&self.values, newest)
            .map(|k| (k * LEVEL_SAMPLES + self.used) / frame.max(1))
    }
}

/// The last VQE chunks as s16, for the pre-roll after a wake word.
pub(crate) struct PrerollRing {
    buf: Vec<i16>,
    frame: usize,
    /// Slot the next chunk goes to.
    next: usize,
    filled: usize,
}

impl PrerollRing {
    pub(crate) fn new(chunks: usize, frame: usize) -> Self {
        PrerollRing {
            buf: vec![0; chunks * frame],
            frame,
            next: 0,
            filled: 0,
        }
    }

    fn chunks(&self) -> usize {
        self.buf.len() / self.frame.max(1)
    }

    /// Stores one chunk and returns it as s16. A chunk of another length breaks the timeline:
    /// the ring is cleared and nothing is stored.
    pub(crate) fn push(&mut self, d: &[f32]) -> Option<&[i16]> {
        if d.len() != self.frame || self.frame == 0 || self.buf.is_empty() {
            self.clear();
            return None;
        }
        let at = self.next * self.frame;
        let slot = &mut self.buf[at..at + self.frame];
        for (o, &v) in slot.iter_mut().zip(d) {
            *o = to_s16(v);
        }
        self.next = (self.next + 1) % self.chunks();
        self.filled = (self.filled + 1).min(self.chunks());
        Some(&self.buf[at..at + self.frame])
    }

    pub(crate) fn clear(&mut self) {
        self.next = 0;
        self.filled = 0;
    }

    /// The last `n` chunks (at most as many as stored), oldest first.
    pub(crate) fn last(&self, n: usize) -> Vec<i16> {
        let n = n.min(self.filled);
        let total = self.chunks();
        let mut out = Vec::with_capacity(n * self.frame);
        for k in (1..=n).rev() {
            let at = (self.next + total - k) % total * self.frame;
            out.extend_from_slice(&self.buf[at..at + self.frame]);
        }
        out
    }
}

/// Chunks from the newest entry of the level window back to the last one within
/// `SPEECH_END_RATIO` of its loudest (0 = the newest itself); None for a silent window.
fn speech_end_chunks(h: &[f32; LEVEL_CHUNKS], newest: usize) -> Option<usize> {
    let peak = h.iter().copied().fold(0.0f32, f32::max);
    if peak <= 0.0 {
        return None;
    }
    let floor = SPEECH_END_RATIO * peak;
    (0..LEVEL_CHUNKS).find(|&k| h[(newest + LEVEL_CHUNKS - k) % LEVEL_CHUNKS] >= floor)
}

/// Pre-roll length in chunks for `rule`, given the chunks since the 1st-stage hit.
fn preroll_chunks(rule: Option<PrerollRule>, first_stage_age: Option<usize>) -> usize {
    match (rule, first_stage_age) {
        (None, _) => 0,
        (Some(PrerollRule::Fixed(n)), _) => n as usize,
        (Some(PrerollRule::FirstStage { offset, .. }), Some(a)) => {
            (a as i64 + i64::from(offset)).max(0) as usize
        }
        (Some(PrerollRule::FirstStage { fallback, .. }), None) => fallback as usize,
    }
}

/// "-" for None in the "wake timing" log.
struct Chunks(Option<usize>);

impl std::fmt::Display for Chunks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(n) => write!(f, "{n}"),
            None => f.write_str("-"),
        }
    }
}

fn processing(
    model: &'static Model,
    mut eng: Engine,
    shared: &Shared,
    pipe: &Pipe,
    tx: &UnboundedSender<FrontendEvent>,
) {
    let period = eng.chunk;
    let nch = model.capture_channels as usize;
    let (mic_ch, spk_ch) = (model.mic_channels, model.spk_channels);
    let nspk = spk_ch.len();
    let mut input = vec![0i16; period * nch];
    let mut mics = vec![0f32; period * mic_ch.len()];
    let mut spk = vec![0f32; period * nspk];
    let mut level_hist = LevelHistory::new();
    let mut ana = SpectrumAnalyzer::new(model.capture_rate, period);
    if !ana.ok() {
        warn!("music analysis: unsupported capture layout");
    }
    let mut music_was_on = false;
    let frame = eng.frame();
    let mut ring = PrerollRing::new(PREROLL_SAMPLES / frame.max(1), frame);
    // Chunks since the wake spotter's 1st-stage hit, while its verifier waits.
    let mut first_stage_age: Option<usize> = None;

    while shared.running() {
        let pending = lock(&shared.pending_model).take();
        if let Some(dir) = pending {
            first_stage_age = None;
            if !eng.set_wake_model(&dir) {
                error!("failed to switch wake word model to {}", dir.display());
            }
        }
        // Exactly one VQE chunk.
        {
            let f = lock(&pipe.fifo);
            let (mut f, _) = pipe
                .ready
                .wait_timeout_while(f, CHUNK_WAIT, |f| !f.has_chunk() && shared.running())
                .unwrap_or_else(PoisonError::into_inner);
            if !f.take_chunk(&mut input) {
                continue;
            }
            shared.fifo_frames.store(f.frames() as u64, Relaxed);
        }
        shared.chunks.fetch_add(1, Relaxed);

        deinterleave(&input, nch, mic_ch, spk_ch, &mut mics, &mut spk);
        let music_on = shared.music.load(Relaxed) && ana.ok();
        if music_on && !music_was_on {
            ana.reset();
        }
        music_was_on = music_on;
        let mut mf = MusicFrame::default();
        if music_on && ana.process(&spk, nspk, &mut mf) {
            let _ = tx.send(FrontendEvent::Music(mf));
        }
        if !eng.process(&mics, &spk) {
            ring.clear();
            continue;
        }

        // VQE_OMNI is cleaned of the station's own playback but has no AGC, so it keeps the
        // distance to the talker; VQE_OMNI_WITH_GAIN, which the spotter hears, normalizes it
        // away.
        if let Some(d) = eng.output(VQE_OMNI)
            && !d.is_empty()
        {
            level_hist.push(d);
        }

        // Every chunk goes into the ring, so a pre-roll never spans a gap. A `Wake` not yet
        // handled holds the stream open: the owner switches streaming on only when it handles
        // the event, and the chunks in between continue the pre-roll. `wake_pending` is read
        // first: its Acquire pairs with `wake_handled`, after the owner's `set_streaming(true)`.
        match eng.output(VQE_OMNI_WITH_GAIN) {
            Some(d) => {
                if let Some(slot) = ring.push(d)
                    && (shared.wake_pending.load(Acquire) > 0 || shared.stream.load(Relaxed))
                {
                    let _ = tx.send(FrontendEvent::Audio(slot.to_vec()));
                }
            }
            None => ring.clear(),
        }

        if shared.detect.load(Relaxed) {
            // Kept for the level window's 1.5 s, whatever the VQE chunk.
            first_stage_age = first_stage_age
                .map(|a| a + 1)
                .filter(|&a| a * frame < LEVEL_CHUNKS * LEVEL_SAMPLES);
            let spotted = eng.feed_wake();
            match spotted.stage {
                Stage::FirstStageHit => first_stage_age = Some(0),
                Stage::Rejected => first_stage_age = None,
                Stage::None => {}
            }
            if !spotted.phrases.is_empty() {
                let level_db = wake_level_db(&level_hist.values);
                let speech_end = level_hist.speech_end(frame);
                let rule = shared
                    .preroll
                    .load(Relaxed)
                    .then(|| model.wake_preroll.iter().find(|p| p.id == eng.wake_id()))
                    .flatten()
                    .map(|p| p.rule);
                let n = preroll_chunks(rule, first_stage_age).min(ring.chunks());
                let preroll = ring.last(n);
                info!(
                    "wake timing: first_stage={} speech_end={} preroll={}",
                    Chunks(first_stage_age),
                    Chunks(speech_end),
                    preroll.len() / frame
                );
                first_stage_age = None;
                for phrase in spotted.phrases {
                    shared.wake_pending.fetch_add(1, Relaxed);
                    let _ = tx.send(FrontendEvent::Wake {
                        phrase,
                        level_db,
                        preroll: preroll.clone(),
                    });
                }
            }
        } else {
            first_stage_age = None;
        }
        if shared.commands.load(Relaxed) {
            for word in eng.feed_command() {
                let _ = tx.send(FrontendEvent::Command { word });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsar_alsa::HwResult;
    use pulsar_core::model::model_from_compatible;

    #[test]
    fn s32_capture_keeps_the_top_16_of_24_bits() {
        // Right-justified 24-bit samples (the Mini, shift 8): the sign survives the shift, and
        // a value outside 24 bits saturates instead of wrapping.
        let src = [
            -1 << 8,
            0x007F_FF00,
            -0x0080_0000,
            0x0000_00FF,
            -1,
            0x0100_0000,
            i32::MIN,
        ];
        let mut dst = [0i16; 7];
        s32_to_s16(&src, 8, &mut dst);
        assert_eq!(dst, [-1, 32767, -32768, 0, -1, 32767, -32768]);
    }

    #[test]
    fn level_is_the_mean_of_the_8_loudest_rounded_to_a_tenth() {
        assert_eq!(wake_level_db(&[0.0; LEVEL_CHUNKS]), -100.0);
        // 8 frames at 0.01 (-20 dB) among quiet ones, anywhere in the ring.
        let mut h = [1e-6f32; LEVEL_CHUNKS];
        for i in [0, 5, 13, 20, 30, 40, 45, 46] {
            h[i] = 0.01;
        }
        assert_eq!(wake_level_db(&h), -20.0);
        // Only the top 8 count: a ninth loud frame changes nothing, a louder one does.
        h[1] = 0.01;
        assert_eq!(wake_level_db(&h), -20.0);
        h[2] = 0.09;
        // (0.09 + 7 * 0.01) / 8 = 0.02 -> -16.99 dB
        assert_eq!(wake_level_db(&h), -17.0);
        // Rounded half away from zero to 0.1 dB: -32.46 -> -32.5, -32.44 -> -32.4.
        let at = |db: f64| [10f64.powf(db / 10.0) as f32; LEVEL_CHUNKS];
        assert_eq!(wake_level_db(&at(-32.46)), -32.5);
        assert_eq!(wake_level_db(&at(-32.44)), -32.4);
        // Clamped to -100..0.
        assert_eq!(wake_level_db(&at(3.0)), 0.0);
        assert_eq!(wake_level_db(&at(-120.0)), -100.0);
    }

    #[test]
    fn level_window_is_independent_of_vqe_chunk_size() {
        let mut full = LevelHistory::new();
        let mut mini3 = LevelHistory::new();
        let mut old = [0.0; LEVEL_CHUNKS];
        for block in 0..LEVEL_CHUNKS + 5 {
            let samples: Vec<f32> = (0..LEVEL_SAMPLES)
                .map(|i| ((block * 17 + i * 3) % 97) as f32 / 128.0)
                .collect();
            let before = mini3.values.map(f32::to_bits);
            mini3.push(&samples[..256]);
            assert_eq!(mini3.values.map(f32::to_bits), before);
            mini3.push(&samples[256..]);
            full.push(&samples);
            old[block % LEVEL_CHUNKS] = mean_square(&samples);
            assert_eq!(full.values.map(f32::to_bits), old.map(f32::to_bits));
            assert_eq!(mini3.values.map(f32::to_bits), old.map(f32::to_bits));
            assert_eq!(mini3.pos, full.pos);
            assert_eq!(wake_level_db(&mini3.values), wake_level_db(&old));
        }
    }

    #[test]
    fn preroll_ring_keeps_the_last_chunks_oldest_first() {
        // 3 chunks of 2 samples; chunk k holds the samples k, k.
        let mut r = PrerollRing::new(3, 2);
        let chunk = |k: i16| [f32::from(k) / 32768.0; 2];
        assert!(r.last(2).is_empty());
        assert_eq!(r.push(&chunk(1)), Some(&[1i16, 1][..]));
        // Asked for more than stored: what there is.
        assert_eq!(r.last(3), [1, 1]);
        for k in 2..=5 {
            r.push(&chunk(k));
        }
        // Wrapped twice: chunks 3, 4, 5 remain.
        assert_eq!(r.last(3), [3, 3, 4, 4, 5, 5]);
        assert_eq!(r.last(2), [4, 4, 5, 5]);
        assert_eq!(r.last(9), [3, 3, 4, 4, 5, 5]);
        assert!(r.last(0).is_empty());
        // A chunk of another length breaks the timeline: nothing older survives it.
        assert_eq!(r.push(&[0.0; 3]), None);
        assert!(r.last(3).is_empty());
        r.push(&chunk(6));
        assert_eq!(r.last(3), [6, 6]);
    }

    #[test]
    fn speech_end_is_the_newest_chunk_within_20_db_of_the_peak() {
        let mut h = [0f32; LEVEL_CHUNKS];
        assert_eq!(speech_end_chunks(&h, 10), None);
        // Word in chunks 3..=8, newest = 12: 4 quiet chunks after it.
        for v in &mut h[3..=8] {
            *v = 1.0;
        }
        h[9] = 0.009;
        assert_eq!(speech_end_chunks(&h, 12), Some(4));
        // Exactly 20 dB down still counts as speech.
        h[9] = 0.01;
        assert_eq!(speech_end_chunks(&h, 12), Some(3));
        // The window is a ring: newest = 1 reaches back over the end to the word.
        assert_eq!(speech_end_chunks(&h, 1), Some(LEVEL_CHUNKS - 9 + 1));
        assert_eq!(speech_end_chunks(&h, 9), Some(0));
    }

    #[test]
    fn preroll_length_per_rule() {
        let first = |offset, fallback| Some(PrerollRule::FirstStage { offset, fallback });
        assert_eq!(preroll_chunks(None, Some(5)), 0);
        assert_eq!(preroll_chunks(Some(PrerollRule::Fixed(7)), None), 7);
        assert_eq!(preroll_chunks(Some(PrerollRule::Fixed(7)), Some(2)), 7);
        assert_eq!(preroll_chunks(first(3, 9), Some(4)), 7);
        assert_eq!(preroll_chunks(first(-3, 9), Some(4)), 1);
        // An anchor closer than the offset: nothing rather than the word itself.
        assert_eq!(preroll_chunks(first(-3, 9), Some(2)), 0);
        // No 1st-stage hit before the confirmation.
        assert_eq!(preroll_chunks(first(-3, 9), None), 9);
    }

    #[test]
    fn deinterleave_supported_layouts() {
        let midi = model_from_compatible(b"yandex,cucumber\0").unwrap();
        let lite2 = model_from_compatible(b"yandex,plum\0").unwrap();
        let mini3 = model_from_compatible(b"yandex,bergamot\0").unwrap();
        for m in [midi, lite2, mini3] {
            let nch = m.capture_channels as usize;
            let frames = 4;
            // Sample = frame * 100 + channel.
            let input: Vec<i16> = (0..frames * nch)
                .map(|s| ((s / nch) * 100 + s % nch) as i16)
                .collect();
            let mut mics = vec![0.0; frames * m.mic_channels.len()];
            let mut spk = vec![0.0; frames * m.spk_channels.len()];
            deinterleave(
                &input,
                nch,
                m.mic_channels,
                m.spk_channels,
                &mut mics,
                &mut spk,
            );
            let v = |frame: usize, ch: u32| (frame * 100 + ch as usize) as f32 / 32768.0;
            for i in 0..frames {
                for (c, &ch) in m.mic_channels.iter().enumerate() {
                    assert_eq!(mics[i * m.mic_channels.len() + c], v(i, ch));
                }
                for (c, &ch) in m.spk_channels.iter().enumerate() {
                    assert_eq!(spk[i * m.spk_channels.len() + c], v(i, ch));
                }
            }
        }
        // Lite 2: mic 0 on ch 0, mic 1 on ch 2, speaker feedback on ch 6.
        let input: Vec<i16> = vec![
            -32768, 1, 16384, 3, 4, 5, 32767, 7, //
            8, 9, 10, 11, 12, 13, 14, 15,
        ];
        let (mut mics, mut spk) = ([0.0; 4], [0.0; 2]);
        deinterleave(
            &input,
            8,
            lite2.mic_channels,
            lite2.spk_channels,
            &mut mics,
            &mut spk,
        );
        assert_eq!(mics, [-1.0, 0.5, 8.0 / 32768.0, 10.0 / 32768.0]);
        assert_eq!(spk, [32767.0 / 32768.0, 14.0 / 32768.0]);
    }

    #[test]
    fn fifo_takes_whole_chunks_in_order() {
        // 2 channels, chunks of 3 frames, at most 10 frames.
        let mut f = Fifo::new(2, 3, 10);
        let mut out = [0i16; 6];
        assert!(!f.take_chunk(&mut out));
        assert_eq!(f.push(&[1, 1, 2, 2]), None);
        assert!(!f.has_chunk());
        assert_eq!(f.push(&[3, 3, 4, 4]), None);
        assert_eq!(f.frames(), 4);
        assert!(f.take_chunk(&mut out));
        assert_eq!(out, [1, 1, 2, 2, 3, 3]);
        assert_eq!(f.frames(), 1);
        // Wrong output size: nothing taken.
        assert_eq!(f.push(&[5, 5, 6, 6]), None);
        assert!(!f.take_chunk(&mut [0; 4]));
        assert!(f.take_chunk(&mut out));
        assert_eq!(out, [4, 4, 5, 5, 6, 6]);
        assert_eq!(f.frames(), 0);
    }

    #[test]
    fn fifo_drops_the_oldest_past_one_second() {
        let mut f = Fifo::new(2, 3, 10);
        let block: Vec<i16> = (0..16).collect();
        assert_eq!(f.push(&block), None);
        // 8 + 4 frames: 2 over the limit, the 2 oldest go.
        let block: Vec<i16> = (16..24).collect();
        assert_eq!(f.push(&block), Some(2));
        assert_eq!(f.frames(), 10);
        let mut out = [0i16; 6];
        assert!(f.take_chunk(&mut out));
        assert_eq!(out, [4, 5, 6, 7, 8, 9]);
        // Exactly at the limit: nothing dropped.
        let mut f = Fifo::new(2, 3, 10);
        assert_eq!(f.push(&[0; 20]), None);
    }

    #[test]
    fn fifo_drops_the_partial_chunk_after_an_error() {
        let mut f = Fifo::new(1, 4, 100);
        f.push(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        f.drop_partial();
        assert_eq!(f.frames(), 8);
        f.drop_partial();
        assert_eq!(f.frames(), 8);
        let mut out = [0i16; 4];
        assert!(f.take_chunk(&mut out));
        assert!(f.take_chunk(&mut out));
        assert_eq!(out, [5, 6, 7, 8]);
        f.push(&[11, 12]);
        f.drop_partial();
        assert_eq!(f.frames(), 0);
    }

    fn trace(rate: u32, failed: Option<(&'static str, i32)>) -> HwTrace {
        let r = HwResult {
            rate,
            period_frames: 1536,
            buffer_frames: 12288,
        };
        HwTrace {
            near: r,
            actual: r,
            failed,
        }
    }

    #[test]
    fn hw_params_checked_in_cxx_order() {
        assert_eq!(hw_outcome(&trace(48000, None), 48000), Ok((1536, 12288)));
        // The rate is checked right after set_rate_near, before period / buffer / apply.
        assert_eq!(
            hw_outcome(&trace(44100, None), 48000),
            Err(HwFail::Rate(44100))
        );
        assert_eq!(
            hw_outcome(&trace(44100, Some(("apply", -5))), 48000),
            Err(HwFail::Rate(44100))
        );
        assert_eq!(
            hw_outcome(&trace(44100, Some(("channels", -22))), 48000),
            Err(HwFail::Step("set_channels", -22))
        );
        assert_eq!(
            hw_outcome(&trace(48000, Some(("rate", -22))), 48000),
            Err(HwFail::Step("set_rate_near", -22))
        );
        assert_eq!(
            hw_outcome(&trace(48000, Some(("buffer", -22))), 48000),
            Err(HwFail::Step("set_buffer_size_near", -22))
        );
        assert_eq!(
            hw_outcome(&trace(48000, Some(("apply", -16))), 48000),
            Err(HwFail::Step("snd_pcm_hw_params", -16))
        );
        // Reading the sizes back was not a C++ step: the near values count.
        assert_eq!(
            hw_outcome(&trace(48000, Some(("get period", -5))), 48000),
            Ok((1536, 12288))
        );
    }

    #[test]
    fn without_the_yandex_library_the_handle_is_degraded() {
        // No /system/vendor/quasar/libs on the build host: dlopen fails.
        let midi = model_from_compatible(b"yandex,cucumber\0").unwrap();
        let (h, mut rx) = spawn(midi, PathBuf::from("/nonexistent"), None);
        h.set_streaming(true);
        h.set_model(PathBuf::from("/nonexistent/2"));
        let h2 = h.clone();
        drop(h);
        h2.set_music_analysis(true);
        assert_eq!(h2.stats(), FrontendStats::default());
        // No event, and the channel stays open while a handle lives.
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        drop(h2);
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
