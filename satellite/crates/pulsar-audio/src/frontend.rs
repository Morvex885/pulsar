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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use log::{error, info, warn};
use pulsar_alsa::{Format, HwRequest, HwTrace, Pcm, Stream};
use pulsar_core::MusicFrame;
use pulsar_core::model::Model;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::engine::{Engine, VQE_OMNI, VQE_OMNI_WITH_GAIN, to_s16};
use crate::spectrum::SpectrumAnalyzer;

/// Wake word level window: 1.5 s of 32 ms VQE frames (512 samples @ 16 kHz on both models),
/// long enough to cover the phrase when the spotter confirms; the loudest frames are the phrase
/// itself.
const LEVEL_CHUNKS: usize = 47;
const LEVEL_TOP: usize = 8;

/// ALSA buffer in VQE chunks.
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
    },
    Command {
        word: String,
    },
    /// 16 kHz mono, one VQE chunk (512 samples), while streaming is on.
    Audio(Vec<i16>),
    /// One per VQE chunk (32 ms) while music analysis is on.
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
        _events: tx.clone(),
    };
    let command_dir = command_model_dir.filter(|d| !d.as_os_str().is_empty());
    let started = match Engine::start(model, &wake_model_dir, command_dir.as_deref()) {
        Some(eng) => inner
            .start_threads(model, eng, tx)
            .inspect_err(|e| error!("audio threads: {e}"))
            .is_ok(),
        None => false,
    };
    if !started {
        inner.stop();
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

fn capture(model: &'static Model, period: usize, shared: &Shared, pipe: &Pipe) {
    let nch = model.capture_channels as usize;
    let mut pcm: Option<Pcm> = None;
    let mut hw_period = period;
    let mut input = vec![0i16; period * nch];
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
        let mut r = p.readi(&mut input, hw_period);
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
        let block = &input[..n * nch];
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

/// Opens the model's capture PCM: S16, its channels, exactly its rate (a silent rate mismatch
/// would feed the VQE and spotters wrongly timed audio), period = one VQE chunk, buffer = 8.
/// The period may differ (the FIFO decouples ALSA periods from VQE chunks). Returns the PCM and
/// the frames to read at once.
fn open_capture(model: &Model, period: usize) -> Option<(Pcm, usize)> {
    let dev = model.capture_pcm;
    // Non-blocking: reads never wait inside ALSA, so a stop never hangs in snd_pcm_readi.
    let mut pcm = match Pcm::open(dev, Stream::Capture, true) {
        Ok(p) => p,
        Err(e) => {
            error!("snd_pcm_open {dev}: {}", pulsar_alsa::strerror(e));
            return None;
        }
    };
    let t = pcm.set_hw_params_traced(&HwRequest {
        format: Format::S16Le,
        channels: model.capture_channels,
        rate: model.capture_rate,
        rate_exact: false,
        period_frames: Some(period as u64),
        buffer_frames: Some(period as u64 * BUFFER_CHUNKS),
    });
    match hw_outcome(&t, model.capture_rate) {
        Ok((per, buf)) => {
            info!(
                "capture {dev}: {} Hz, {} ch, period {per}, buffer {buf}",
                t.near.rate, model.capture_channels
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
    // Mean square per VQE frame.
    let mut level_hist = [0f32; LEVEL_CHUNKS];
    let mut level_pos = 0;
    let mut ana = SpectrumAnalyzer::new(model.capture_rate, period);
    if !ana.ok() {
        warn!("music analysis: unsupported capture layout");
    }
    let mut music_was_on = false;

    while shared.running() {
        let pending = lock(&shared.pending_model).take();
        if let Some(dir) = pending
            && !eng.set_wake_model(&dir)
        {
            error!("failed to switch wake word model to {}", dir.display());
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
            continue;
        }

        // VQE_OMNI is cleaned of the station's own playback but has no AGC, so it keeps the
        // distance to the talker; VQE_OMNI_WITH_GAIN, which the spotter hears, normalizes it
        // away.
        if let Some(d) = eng.output(VQE_OMNI)
            && !d.is_empty()
        {
            level_hist[level_pos] = mean_square(d);
            level_pos = (level_pos + 1) % LEVEL_CHUNKS;
        }

        if shared.stream.load(Relaxed)
            && let Some(d) = eng.output(VQE_OMNI_WITH_GAIN)
            && !d.is_empty()
        {
            let _ = tx.send(FrontendEvent::Audio(d.iter().map(|&v| to_s16(v)).collect()));
        }

        if shared.detect.load(Relaxed) {
            let phrases = eng.feed_wake();
            if !phrases.is_empty() {
                let level_db = wake_level_db(&level_hist);
                for phrase in phrases {
                    let _ = tx.send(FrontendEvent::Wake { phrase, level_db });
                }
            }
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
    fn deinterleave_midi_and_lite2_layouts() {
        let midi = model_from_compatible(b"yandex,cucumber\0").unwrap();
        let lite2 = model_from_compatible(b"yandex,plum\0").unwrap();
        for m in [midi, lite2] {
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
