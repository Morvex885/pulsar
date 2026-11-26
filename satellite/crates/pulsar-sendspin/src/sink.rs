//! ALSA output of the Sendspin player on the model's playback PCM (dmix, mixed with the GStreamer
//! channels). `configure` / `release` run on the runtime thread (stream start / end), `write` on
//! the library's sync task; the PCM is opened lazily by `write`, so the runtime thread never
//! blocks on it. `io` is held by `write` for its whole duration, including the snd_pcm_wait
//! slices, so the runtime thread never takes it blocking: configure / release only record a
//! request (`req`, held for a few instructions) and apply it right away if the audio thread is
//! idle (try_lock); otherwise the audio thread applies it at its next wait slice and abandons the
//! write in progress.

use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::sync::{Mutex, MutexGuard, PoisonError};

use log::{debug, error, info};
use pulsar_alsa::{Format, HwRequest, Pcm};
use pulsar_core::model::Model;

use crate::ffi::now_us;
use crate::gain;

/// Longest snd_pcm_wait() slice inside one write.
const WAIT_SLICE_MS: i64 = 20;
const STATS_PERIOD_US: i64 = 10_000_000;
const REOPEN_RETRY_US: i64 = 1_000_000;

#[derive(Clone, Copy)]
enum Req {
    None,
    Configure { rate: u32, channels: u8, bits: u8 },
    Release,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Output {
    Closed,
    Ready,
    Reported,
}

struct Request {
    pending: Req,
    // Владелец выхода сохраняется и после применения запроса к ALSA.
    start: Option<u64>,
    output: Output,
}

pub(crate) struct Sink {
    io: Mutex<Io>,
    req: Mutex<Request>,
    req_gen: AtomicU32,
    applied_gen: AtomicU32,
    /// f32 bits of the duck gain the output ramps to, 0..1.
    target_gain: AtomicU32,
}

/// Audio thread state (under `Sink::io`).
struct Io {
    /// Playback PCM and output gates of the model.
    model: &'static Model,
    pcm: Option<Pcm>,
    active: bool,
    output_ready: bool,
    want_rate: u32,
    rate: u32,
    want_channels: u8,
    channels: u8,
    want_bits: u8,
    bits: u8,
    open_failed_logged: bool,
    next_open_us: i64,
    /// Frames consumed without reaching the device, not yet reported.
    discarded: u32,
    gain: f32,
    scratch: Vec<u8>,
    // stats
    frames_total: u64,
    xruns: u32,
    last_delay_frames: i64,
    delay_min: i64,
    delay_max: i64,
    first_write_us: i64,
    prev_stats_us: i64,
    prev_played: i64,
    stats_us: i64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Sink {
    pub(crate) fn new(model: &'static Model) -> Self {
        Self {
            io: Mutex::new(Io {
                model,
                pcm: None,
                active: false,
                output_ready: false,
                want_rate: 0,
                rate: 0,
                want_channels: 0,
                channels: 0,
                want_bits: 0,
                bits: 0,
                open_failed_logged: false,
                next_open_us: 0,
                discarded: 0,
                gain: 1.0,
                scratch: Vec::new(),
                frames_total: 0,
                xruns: 0,
                last_delay_frames: 0,
                delay_min: i64::from(i32::MAX),
                delay_max: 0,
                first_write_us: 0,
                prev_stats_us: 0,
                prev_played: 0,
                stats_us: 0,
            }),
            req: Mutex::new(Request {
                pending: Req::None,
                start: None,
                output: Output::Closed,
            }),
            req_gen: AtomicU32::new(0),
            applied_gen: AtomicU32::new(0),
            target_gain: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    pub(crate) fn configure(&self, rate: u32, channels: u8, bits: u8) -> u64 {
        let order = {
            let mut req = lock(&self.req);
            let order = pulsar_core::audio_start_order();
            self.record(
                &mut req,
                Req::Configure {
                    rate,
                    channels,
                    bits,
                },
                Some(order),
            );
            order
        };
        self.try_apply();
        order
    }

    pub(crate) fn owns_start(&self, order: u64) -> bool {
        lock(&self.req).start == Some(order)
    }

    pub(crate) fn take_ready_start(&self) -> Option<u64> {
        let mut req = lock(&self.req);
        if req.output != Output::Ready {
            return None;
        }
        req.output = Output::Reported;
        req.start
    }

    pub(crate) fn output_is_current(&self, order: u64) -> bool {
        let req = lock(&self.req);
        req.start == Some(order) && req.output != Output::Closed
    }

    /// Stream ended: drop what is queued and release the device so dmix is not held while idle.
    pub(crate) fn release(&self) {
        self.record(&mut lock(&self.req), Req::Release, None);
        self.try_apply();
    }

    /// Закрывает допуск синхронно; сам PCM закроет sync task или `finish_request`.
    pub(crate) fn revoke_before(&self, order: u64) -> bool {
        let mut req = lock(&self.req);
        if !req.start.is_some_and(|start| start < order) {
            return false;
        }
        self.record(&mut req, Req::Release, None);
        true
    }

    /// Вызывается вне LocalSet, если новых записей может не быть.
    pub(crate) fn finish_request(&self) {
        if self.pending() {
            self.apply_pending(&mut lock(&self.io));
        }
    }

    pub(crate) fn set_gain(&self, g: f32) {
        let g = g.clamp(0.0, 1.0);
        self.target_gain.store(g.to_bits(), SeqCst);
        // No stream yet (or the device is closed): start the first frame at the new gain instead
        // of ramping down from 1.0. A desktop/phone volume change before playback is immediate.
        //
        // Never block on `io`: the sync thread holds it across a whole `write` (including
        // `pcm.wait`), and this runs in the controller's event loop on every volume step; a
        // blocking lock here would stall every channel until the write returns. A running
        // stream picks `target_gain` up in its ramp anyway.
        if let Ok(mut io) = self.io.try_lock()
            && (!io.active || io.pcm.is_none())
        {
            io.gain = g;
        }
    }

    fn target_gain(&self) -> f32 {
        f32::from_bits(self.target_gain.load(SeqCst))
    }

    /// A write now would take the audio: a stream is configured and no request is pending.
    #[cfg(test)]
    pub(crate) fn accepts_audio(&self) -> bool {
        !self.pending() && lock(&self.io).active
    }

    /// Sync task: (bytes consumed, frames played to report, their finish time in µs).
    pub(crate) fn write(&self, data: &[u8], timeout_ms: u32) -> (usize, u32, i64) {
        let generation = self.req_gen.load(SeqCst);
        let r = self.write_once(data, timeout_ms, generation);
        // A request that arrived while `io` was held (request()'s try_lock failed) must not wait
        // for the next write: a release may be the last event, and the device would stay open.
        self.finish_request();
        r
    }

    fn pending(&self) -> bool {
        self.req_gen.load(SeqCst) != self.applied_gen.load(SeqCst)
    }

    fn write_once(&self, data: &[u8], timeout_ms: u32, generation: u32) -> (usize, u32, i64) {
        let length = data.len();
        let deadline = now_us() + i64::from(timeout_ms) * 1000;
        let mut frames_done: u32 = 0;
        let mut finish_us = 0i64;
        let mut report = 0u32;
        let mut io = lock(&self.io);
        self.apply_pending(&mut io);
        let bpf = usize::from(io.want_channels) * usize::from(io.want_bits / 8);
        if !io.active || bpf == 0 || self.applied_gen.load(SeqCst) != generation || self.pending() {
            return (length, 0, 0); // stale audio after stream end: discard
        }
        if io.pcm.is_none() && !io.open() {
            // Device unavailable: discard rather than spin the task; the gap is reported as
            // played with the next timed report so the library's buffered-frames estimate stays
            // right.
            io.discarded = io.discarded.wrapping_add((length / bpf) as u32);
            return (length - length % bpf, 0, 0);
        }
        let frames_total = length / bpf;
        let mut scratch = std::mem::take(&mut io.scratch);
        let src: &[u8] = if io.scale(data, frames_total, self.target_gain(), &mut scratch) {
            &scratch
        } else {
            data
        };

        let mut first = true;
        while (frames_done as usize) < frames_total {
            if self.pending() {
                break;
            }
            let Some(pcm) = io.pcm.as_mut() else { break };
            if !first && now_us() >= deadline {
                break;
            }
            first = false;
            let avail = match pcm.avail_update() {
                Ok(n) => n,
                Err(e) => {
                    if !io.recover(e) {
                        break;
                    }
                    continue;
                }
            };
            if avail == 0 {
                let left_ms = (deadline - now_us()) / 1000;
                if left_ms <= 0 {
                    break;
                }
                if let Err(e) = pcm.wait(left_ms.min(WAIT_SLICE_MS) as i32)
                    && !io.recover(e)
                {
                    break;
                }
                continue;
            }
            let done = frames_done as usize;
            let chunk = (frames_total - done).min(avail);
            if self.pending() {
                break;
            }
            match pcm.writei(&src[done * bpf..], chunk) {
                Ok(w) => frames_done += w as u32,
                Err(e) => {
                    if !io.recover(e) {
                        break;
                    }
                }
            }
        }
        io.scratch = scratch;
        if self.pending() {
            // Буфер относится к отозванному потоку, не переносить остаток в новый.
            self.apply_pending(&mut io);
            return (length, 0, 0);
        }
        // Commit the ramp by the frames actually written; an unwritten tail is presented again.
        io.gain = gain::ramp_end_gain(io.gain, self.target_gain(), io.rate, frames_done);

        if frames_done > 0 {
            // The frames just written finish playing once everything queued ahead of the DAC
            // (our ring + plug/rate converter + dmix ring) has played: now + snd_pcm_delay().
            if let Some(Ok(delay)) = io.pcm.as_mut().map(Pcm::delay)
                && delay >= 0
            {
                let now = now_us();
                finish_us = now + delay * 1_000_000 / i64::from(io.rate);
                io.last_delay_frames = delay;
                io.delay_min = io.delay_min.min(delay);
                io.delay_max = io.delay_max.max(delay);
                if io.first_write_us == 0 {
                    io.first_write_us = now;
                }
            }
        }
        io.frames_total += u64::from(frames_done);
        io.maybe_log_stats();
        let lost = io.pcm.is_none();
        self.update_output_after_write(&mut io, generation, frames_done, lost);
        if lost && frames_done == 0 {
            io.discarded = io.discarded.wrapping_add((length / bpf) as u32);
        }
        if finish_us != 0 {
            report = frames_done.wrapping_add(io.discarded);
            io.discarded = 0;
        }
        drop(io);
        if lost && frames_done == 0 {
            return (length - length % bpf, report, finish_us); // device lost mid-write: discard
        }
        (frames_done as usize * bpf, report, finish_us)
    }

    fn update_output_after_write(&self, io: &mut Io, generation: u32, written: u32, lost: bool) {
        if written == 0 && !lost {
            return;
        }
        // Даже переиспользованный PCM подтверждает запуск только после записи кадров.
        // Блокировка нужна лишь при изменении готовности, не на каждой записи.
        let ready = !lost;
        if std::mem::replace(&mut io.output_ready, ready) == ready {
            return;
        }
        let mut req = lock(&self.req);
        if self.req_gen.load(SeqCst) == generation && req.start.is_some() {
            req.output = if ready { Output::Ready } else { Output::Closed };
        }
    }

    fn try_apply(&self) {
        let io = match self.io.try_lock() {
            Ok(io) => Some(io),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        if let Some(mut io) = io {
            self.apply_pending(&mut io);
        }
    }

    fn record(&self, req: &mut Request, pending: Req, start: Option<u64>) {
        *req = Request {
            pending,
            start,
            output: Output::Closed,
        };
        self.req_gen.fetch_add(1, SeqCst);
    }

    fn apply_pending(&self, io: &mut Io) {
        let r = {
            let mut req = lock(&self.req);
            self.applied_gen.store(self.req_gen.load(SeqCst), SeqCst);
            std::mem::replace(&mut req.pending, Req::None)
        };
        match r {
            Req::None => {}
            Req::Configure {
                rate,
                channels,
                bits,
            } => io.configure(rate, channels, bits, self.target_gain()),
            Req::Release => io.release(),
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.io
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .close();
    }
}

impl Io {
    fn configure(&mut self, rate: u32, channels: u8, bits: u8, target_gain: f32) {
        if self.pcm.is_some()
            && (rate != self.rate || channels != self.channels || bits != self.bits)
        {
            self.close();
        }
        if let Some(pcm) = self.pcm.as_mut() {
            // Same format: restart from an empty ring.
            let _ = pcm.drop_frames();
            if pcm.prepare().is_err() {
                self.close();
            }
        }
        self.want_rate = rate;
        self.want_channels = channels;
        self.want_bits = bits;
        self.active = true;
        self.output_ready = false;
        self.open_failed_logged = false;
        self.next_open_us = 0;
        self.discarded = 0;
        self.gain = target_gain; // a new stream starts at the current duck level, no ramp
    }

    fn release(&mut self) {
        self.active = false;
        self.output_ready = false;
        if let Some(pcm) = self.pcm.as_mut() {
            let secs = if self.rate != 0 {
                self.frames_total as f64 / f64::from(self.rate)
            } else {
                0.0
            };
            info!(
                "sendspin: stream summary: {secs:.1} s written to the device, xruns {}",
                self.xruns
            );
            let _ = pcm.drop_frames();
        }
        self.close();
    }

    fn open(&mut self) -> bool {
        let now = now_us();
        if now < self.next_open_us {
            return false;
        }
        let format = match self.want_bits {
            16 => Format::S16Le,
            24 => Format::S24_3Le,
            32 => Format::S32Le,
            bits => {
                error!("sendspin: unsupported bit depth {bits}");
                self.next_open_us = now + REOPEN_RETRY_US;
                return false;
            }
        };
        let mut opened = false;
        let res = pulsar_alsa::open_playback(self.model)
            .map_err(|e| ("open", e))
            .and_then(|mut pcm| {
                opened = true;
                let t = pcm.set_hw_params_traced(&HwRequest {
                    format,
                    channels: u32::from(self.want_channels),
                    rate: self.want_rate,
                    rate_exact: true,
                    period_frames: None,
                    buffer_frames: None,
                });
                // Unlike C++, a failed read of period / buffer after the applied hw_params also
                // fails the open (as "hw_params"): pulsar-alsa sets the frame size only after
                // both reads, so the PCM could not be written anyway. Not seen on the stations.
                if let Some((step, e)) = t.failed {
                    return Err((hw_step(step), e));
                }
                let (period, buffer) = (t.actual.period_frames, t.actual.buffer_frames);
                // Start as soon as one period is queued so snd_pcm_delay() tracks real playback
                // early.
                pcm.set_sw_params(period.max(1), period.max(1))
                    .map_err(|e| ("sw_params", e))?;
                pcm.prepare().map_err(|e| ("prepare", e))?;
                Ok((pcm, period, buffer))
            });
        let (pcm, period, buffer) = match res {
            Ok(r) => r,
            Err((step, err)) => {
                if !self.open_failed_logged {
                    error!(
                        "sendspin: ALSA '{}' {step} failed for {} Hz/{} ch/{} bit: {}",
                        self.model.playback_pcm,
                        self.want_rate,
                        self.want_channels,
                        self.want_bits,
                        pulsar_alsa::strerror(err)
                    );
                }
                self.open_failed_logged = true;
                if opened {
                    info!("sendspin: ALSA '{}' closed", self.model.playback_pcm);
                }
                self.next_open_us = now + REOPEN_RETRY_US;
                return false;
            }
        };
        self.pcm = Some(pcm);
        self.rate = self.want_rate;
        self.channels = self.want_channels;
        self.bits = self.want_bits;
        self.xruns = 0;
        self.frames_total = 0;
        self.first_write_us = 0;
        self.prev_stats_us = 0;
        self.prev_played = 0;
        self.delay_min = i64::from(i32::MAX);
        self.delay_max = 0;
        self.stats_us = now;
        info!(
            "sendspin: ALSA '{}' open {} Hz/{} ch/{} bit (period {period}, buffer {buffer} frames = {:.0} ms)",
            self.model.playback_pcm,
            self.rate,
            self.channels,
            self.bits,
            buffer as f64 * 1000.0 / f64::from(self.rate)
        );
        true
    }

    fn close(&mut self) {
        if self.pcm.take().is_some() {
            info!("sendspin: ALSA '{}' closed", self.model.playback_pcm);
        }
    }

    fn recover(&mut self, err: i32) -> bool {
        if err == -libc::EINTR || err == -libc::EAGAIN {
            return true;
        }
        let mut err = err;
        if (err == -libc::EPIPE || err == -libc::ESTRPIPE)
            && let Some(pcm) = self.pcm.as_mut()
        {
            self.xruns += 1;
            let mut r = if err == -libc::ESTRPIPE {
                pcm.resume()
            } else {
                Err(-1)
            };
            if r.is_err() {
                r = pcm.prepare();
            }
            match r {
                Ok(()) => {
                    debug!(
                        "sendspin: ALSA {} recovered",
                        if err == -libc::EPIPE {
                            "underrun"
                        } else {
                            "suspend"
                        }
                    );
                    return true;
                }
                Err(e) => err = e,
            }
        }
        error!(
            "sendspin: ALSA error {}, closing device",
            pulsar_alsa::strerror(err)
        );
        self.close();
        self.next_open_us = now_us() + REOPEN_RETRY_US;
        false
    }

    /// Applies the (ramping) duck gain into `out`; false: write `data` as is.
    fn scale(&self, data: &[u8], frames: usize, target: f32, out: &mut Vec<u8>) -> bool {
        if self.gain == target && target >= 1.0 {
            return false;
        }
        if self.bits != 16 {
            return false; // only 16-bit streams are advertised
        }
        gain::scale_s16(
            data,
            frames,
            usize::from(self.channels),
            self.gain,
            target,
            self.rate,
            out,
        );
        true
    }

    fn maybe_log_stats(&mut self) {
        if now_us() - self.stats_us < STATS_PERIOD_US {
            return;
        }
        self.log_stats("sink");
    }

    /// Frames that reached the DAC since the last report (written minus still queued) against
    /// the wall-clock time between the two reports. The ratio (ppm) is the net rate the
    /// library's sync fed in this interval: server clock vs. DAC clock drift plus soft/hard sync
    /// corrections. Delay min/max show how full the device stayed.
    fn log_stats(&mut self, what: &str) {
        let now = now_us();
        self.stats_us = now;
        if self.rate == 0 || self.first_write_us == 0 {
            return;
        }
        let rate = f64::from(self.rate);
        let played = self.frames_total as i64 - self.last_delay_frames;
        let interval = self.prev_stats_us != 0;
        let wall = (now
            - if interval {
                self.prev_stats_us
            } else {
                self.first_write_us
            }) as f64
            / 1e6;
        let dplayed = (played - if interval { self.prev_played } else { 0 }) as f64 / rate;
        let ppm = if wall > 0.5 {
            (dplayed / wall - 1.0) * 1e6
        } else {
            0.0
        };
        info!(
            "sendspin: {what} {} Hz/{} ch: {:.1} s total, last {wall:.1} s: {dplayed:.3} s played ({ppm:+.0} ppm), output delay {:.1} ms (min {:.1}, max {:.1}), xruns {}, duck gain {:.2}",
            self.rate,
            self.channels,
            played as f64 / rate,
            self.last_delay_frames as f64 * 1000.0 / rate,
            self.delay_min as f64 * 1000.0 / rate,
            self.delay_max as f64 * 1000.0 / rate,
            self.xruns,
            self.gain
        );
        self.prev_stats_us = now;
        self.prev_played = played;
        self.delay_min = i64::from(i32::MAX);
        self.delay_max = 0;
    }
}

/// The C++ sink's step names for pulsar-alsa's hw_params steps.
fn hw_step(step: &'static str) -> &'static str {
    match step {
        "any" => "hw_params_any",
        "access" | "format" | "channels" | "rate" => step,
        _ => "hw_params",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preemption_invalidates_only_the_older_write_generation() {
        let model = pulsar_core::model::model_from_dt_id(b"axg_s420_v03").unwrap();
        let sink = Sink::new(model);
        let first = sink.configure(48000, 2, 16);
        let old = sink.req_gen.load(SeqCst);
        let local = pulsar_core::audio_start_order();

        let io = lock(&sink.io);
        assert!(!sink.revoke_before(first));
        assert!(!sink.pending());
        assert!(sink.revoke_before(local));
        assert!(sink.pending());
        drop(io);
        sink.finish_request();
        assert!(!sink.accepts_audio());

        let next = sink.configure(48000, 2, 16);
        assert!(!sink.revoke_before(local));
        assert!(sink.owns_start(next));
        assert_eq!(sink.write_once(&[1; 4], 0, old), (4, 0, 0));
        assert!(sink.accepts_audio());

        // Пока задача синхронизации ещё пишет в кольцо, configure заменяет отложенный release.
        let io = lock(&sink.io);
        assert!(sink.revoke_before(pulsar_core::audio_start_order()));
        let latest = sink.configure(48000, 2, 16);
        drop(io);
        sink.finish_request();
        assert!(sink.owns_start(latest));
        assert!(sink.accepts_audio());
    }

    #[test]
    fn readiness_requires_written_frames_from_the_current_generation() {
        let model = pulsar_core::model::model_from_dt_id(b"axg_s420_v03").unwrap();
        let sink = Sink::new(model);
        let first = sink.configure(48000, 2, 16);
        sink.test_write_result(0, true);
        sink.test_write_result(0, false);
        assert_eq!(sink.take_ready_start(), None);
        assert!(!sink.output_is_current(first));

        sink.test_write_result(1, false);
        assert_eq!(sink.take_ready_start(), Some(first));
        sink.test_write_result(1, false);
        assert_eq!(sink.take_ready_start(), None);
        assert!(sink.output_is_current(first));

        // Настройка того же формата может прийти, пока предыдущая запись держит io.
        sink.test_write_result(0, true);
        let old = sink.req_gen.load(SeqCst);
        let mut io = lock(&sink.io);
        let next = sink.configure(48000, 2, 16);
        sink.update_output_after_write(&mut io, old, 1, false);
        assert_eq!(sink.take_ready_start(), None);
        drop(io);
        sink.finish_request();
        sink.test_write_result(0, false);
        assert!(!sink.output_is_current(next));
        sink.test_write_result(1, false);
        assert_eq!(sink.take_ready_start(), Some(next));

        sink.test_write_result(0, true);
        assert!(!sink.output_is_current(next));
        sink.test_write_result(0, false);
        assert_eq!(sink.take_ready_start(), None);
        sink.test_write_result(1, false);
        assert_eq!(sink.take_ready_start(), Some(next));
    }

    impl Sink {
        pub(crate) fn test_write_result(&self, written: u32, lost: bool) {
            let mut io = lock(&self.io);
            self.update_output_after_write(&mut io, self.req_gen.load(SeqCst), written, lost);
        }
    }
}
