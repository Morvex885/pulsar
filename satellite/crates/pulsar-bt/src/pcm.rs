//! Вывод входящего A2DP: один поток ALSA на все поколения сеанса.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pulsar_alsa::{Format, HwRequest, Pcm, Stream};
use pulsar_core::model::Model;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, watch};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const FRAMES: usize = 480;
const WAIT_MS: i32 = 10;
const IDLE_AFTER: Duration = Duration::from_millis(250);
const UNITY: [f32; CHANNELS] = [1.0; CHANNELS];

type PcmError = (&'static str, i32);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Started(u64),
    Stopped(u64),
    Activity(u64, u64),
    Idle(u64),
    Failed(u64, String),
}

pub(super) struct Worker {
    shared: Arc<Shared>,
    events: UnboundedSender<Event>,
    terminated: watch::Receiver<bool>,
}

struct Request {
    generation: u64,
    // Забирается потоком один раз, но поколение остаётся в затворе до отзыва.
    address: Option<String>,
}

struct Control {
    request: Option<Request>,
    shutdown: bool,
    admitted: bool,
    gate: u64,
    station_gain: f32,
    phone_gain: [f32; CHANNELS],
    gain: [f32; CHANNELS],
}

impl Control {
    fn generation(&self) -> Option<u64> {
        self.request.as_ref().map(|r| r.generation)
    }

    fn admit(&mut self, admitted: bool) {
        if self.admitted && !admitted {
            // Даже если затвор сразу откроется, очередь старого допуска уже недействительна.
            self.gate = self.gate.wrapping_add(1);
        }
        self.admitted = admitted;
    }

    fn update_gain(&mut self) {
        self.gain = self.phone_gain.map(|gain| gain * self.station_gain);
    }
}

struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
}

impl Shared {
    fn new() -> Self {
        Self {
            control: Mutex::new(Control {
                request: None,
                shutdown: false,
                admitted: false,
                gate: 0,
                station_gain: 1.0,
                phone_gain: UNITY,
                gain: UNITY,
            }),
            wake: Condvar::new(),
        }
    }

    fn next(&self) -> Option<(u64, String)> {
        let mut ctl = self.control.lock().unwrap();
        loop {
            if ctl.shutdown {
                return None;
            }
            if let Some(r) = &mut ctl.request
                && let Some(address) = r.address.take()
            {
                return Some((r.generation, address));
            }
            ctl = self.wake.wait(ctl).unwrap();
        }
    }

    fn gate(&self, generation: u64) -> Option<(u64, bool)> {
        let ctl = self.control.lock().unwrap();
        (ctl.generation() == Some(generation)).then_some((ctl.gate, ctl.admitted))
    }

    fn write_if_admitted<T>(
        &self,
        generation: u64,
        gate: u64,
        write: impl FnOnce([f32; CHANNELS]) -> T,
    ) -> Option<T> {
        // Проверка и неблокирующая запись под одним замком: после отзыва старый
        // поток уже не войдёт в writei. Открытие, ожидание и сброс ALSA — вне замка.
        let ctl = self.control.lock().unwrap();
        if ctl.generation() != Some(generation) || ctl.gate != gate || !ctl.admitted {
            return None;
        }
        Some(write(ctl.gain))
    }
}

impl Worker {
    pub(super) async fn spawn(
        model: &'static Model,
    ) -> io::Result<(Self, UnboundedReceiver<Event>)> {
        let shared = Arc::new(Shared::new());
        let (events, rx) = mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (terminated_tx, terminated) = watch::channel(false);
        let thread_shared = Arc::clone(&shared);
        let thread_events = events.clone();
        std::thread::Builder::new()
            .name("bluetooth-pcm".into())
            .spawn(move || {
                // Тот же выход и формат, что у сеанса. Закрываем PCM до ответа и не пишем звук.
                let probe = open_output(model).map(drop).map_err(|(op, errno)| {
                    let kind = io::Error::from_raw_os_error(-errno).kind();
                    io::Error::new(kind, format!("{op}: {}", pulsar_alsa::strerror(errno)))
                });
                let ready = probe.is_ok();
                if ready_tx.send(probe).is_ok() && ready {
                    run(model, &thread_shared, &thread_events);
                }
                drop(thread_events);
                drop(thread_shared);
                let _ = terminated_tx.send(true);
            })?;

        // Отмена ожидания тоже отзывает worker, даже если ответ уже был отправлен.
        let worker = Self {
            shared,
            events,
            terminated,
        };
        ready_rx
            .await
            .map_err(|_| io::Error::other("поток Bluetooth PCM завершился до проверки выхода"))??;
        Ok((worker, rx))
    }

    pub(super) async fn shutdown(&self) {
        self.signal_stop();
        let _ = self
            .terminated
            .clone()
            .wait_for(|terminated| *terminated)
            .await;
    }

    fn signal_stop(&self) {
        {
            let mut ctl = self.shared.control.lock().unwrap();
            ctl.admit(false);
            ctl.request = None;
            ctl.shutdown = true;
        }
        self.shared.wake.notify_one();
    }

    /// Поколение должно быть уникальным. Сначала оно закрыто для вывода.
    pub(super) fn start(&self, generation: u64, address: String) {
        self.replace(Some(Request {
            generation,
            address: Some(address),
        }));
    }

    pub(super) fn stop(&self) {
        self.replace(None);
    }

    fn replace(&self, request: Option<Request>) {
        let pending = {
            let mut ctl = self.shared.control.lock().unwrap();
            if ctl.shutdown {
                return;
            }
            ctl.admit(false);
            ctl.phone_gain = UNITY;
            ctl.update_gain();
            std::mem::replace(&mut ctl.request, request)
        };
        if let Some(old) = pending
            && old.address.is_some()
        {
            // Поток не забрал эту заявку: у неё не было открытых PCM.
            let _ = self.events.send(Event::Stopped(old.generation));
        }
        self.shared.wake.notify_one();
    }

    pub(super) fn set_station_gain(&self, gain: f32) {
        let mut ctl = self.shared.control.lock().unwrap();
        ctl.station_gain = gain.clamp(0.0, 1.0);
        ctl.update_gain();
    }

    pub(super) fn set_phone_gain(&self, gain: [f32; CHANNELS]) {
        let mut ctl = self.shared.control.lock().unwrap();
        ctl.phone_gain = gain.map(|g| g.clamp(0.0, 1.0));
        ctl.update_gain();
    }

    pub(super) fn set_admitted(&self, admitted: bool) {
        self.shared.control.lock().unwrap().admit(admitted);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

fn run(model: &Model, shared: &Shared, events: &UnboundedSender<Event>) {
    while let Some((generation, address)) = shared.next() {
        let result = session(model, shared, events, generation, &address);
        let event = match result {
            Err((op, errno)) if shared.gate(generation).is_some() => Event::Failed(
                generation,
                format!("{op}: {}", pulsar_alsa::strerror(errno)),
            ),
            _ => Event::Stopped(generation),
        };
        // Сюда приходим только после сброса очереди и закрытия обоих PCM.
        if events.send(event).is_err() {
            break;
        }
    }
}

fn capture_name(address: &str) -> Option<String> {
    let bytes = address.as_bytes();
    (bytes.len() == 17
        && bytes.iter().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                *b == b':'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then(|| format!("bluealsa:DEV={address},PROFILE=a2dp"))
}

fn configure(pcm: &mut Pcm, stream: Stream, context: &'static str) -> Result<(), PcmError> {
    let result = (|| {
        let capture = matches!(stream, Stream::Capture);
        let hw = pcm.set_hw_params(&HwRequest {
            format: Format::S16Le,
            channels: CHANNELS as u32,
            rate: RATE,
            rate_exact: true,
            period_frames: capture.then_some(FRAMES as u64),
            buffer_frames: None,
        })?;
        let period = hw.period_frames.max(1);
        match stream {
            Stream::Capture => pcm.set_sw_params(1, period.min(FRAMES as u64))?,
            Stream::Playback => pcm.set_sw_params(period, period)?,
        }
        pcm.prepare()
    })();
    result.map_err(|e| (context, e))
}

fn open_output(model: &Model) -> Result<Pcm, PcmError> {
    let mut output = pulsar_alsa::open_playback_nonblock(model).map_err(|e| ("выход ALSA", e))?;
    configure(&mut output, Stream::Playback, "формат выхода ALSA")?;
    Ok(output)
}

fn session(
    model: &Model,
    shared: &Shared,
    events: &UnboundedSender<Event>,
    generation: u64,
    address: &str,
) -> Result<(), PcmError> {
    if shared.gate(generation).is_none() {
        return Ok(());
    }
    let name = capture_name(address).ok_or(("адрес A2DP", -libc::EINVAL))?;
    let mut capture = Pcm::open(&name, Stream::Capture, true).map_err(|e| ("вход A2DP", e))?;
    configure(&mut capture, Stream::Capture, "формат входа A2DP")?;
    if shared.gate(generation).is_none() {
        return Ok(());
    }
    let mut output = open_output(model)?;
    let result =
        if shared.gate(generation).is_some() && events.send(Event::Started(generation)).is_ok() {
            transfer(&mut capture, &mut output, shared, events, generation)
        } else {
            Ok(())
        };
    // Даже при ошибке не дожидаемся проигрывания оставшихся кадров.
    if let Err(e) = output.drop_frames() {
        log::warn!(
            "bt: не удалось сбросить очередь ALSA: {}",
            pulsar_alsa::strerror(e)
        );
    }
    result
}

enum Activity {
    AwaitingFirstFrame(Instant),
    Active(Instant),
    Idle,
}

impl Activity {
    fn observe(&mut self, generation: u64, data: bool, now: Instant) -> Option<Event> {
        if data {
            let was_active = matches!(*self, Self::Active(_));
            *self = Self::Active(now);
            return (!was_active)
                .then(|| Event::Activity(generation, pulsar_core::audio_start_order()));
        }
        match *self {
            Self::AwaitingFirstFrame(since) | Self::Active(since)
                if now.duration_since(since) >= IDLE_AFTER =>
            {
                *self = Self::Idle;
                Some(Event::Idle(generation))
            }
            _ => None,
        }
    }
}

fn transfer(
    capture: &mut Pcm,
    output: &mut Pcm,
    shared: &Shared,
    events: &UnboundedSender<Event>,
    generation: u64,
) -> Result<(), PcmError> {
    let mut input = [0; FRAMES * CHANNELS];
    let mut scaled = [0; FRAMES * CHANNELS];
    let mut activity = Activity::AwaitingFirstFrame(Instant::now());
    let mut queued_gate = None;
    while let Some((gate, admitted)) = shared.gate(generation) {
        if queued_gate.is_some_and(|previous| previous != gate) {
            output.drop_frames().map_err(|e| ("сброс выхода ALSA", e))?;
            output
                .prepare()
                .map_err(|e| ("подготовка выхода ALSA", e))?;
            queued_gate = None;
        }
        let result = capture.readi(&mut input, FRAMES);
        let frames = finish_io(capture, result).map_err(|e| ("чтение A2DP", e))?;
        if let Some(event) = activity.observe(generation, frames != 0, Instant::now())
            && events.send(event).is_err()
        {
            return Ok(());
        }
        if !admitted || frames == 0 {
            continue;
        }
        let mut done = 0;
        let mut scaled_gain = UNITY;
        while done < frames {
            let pending = (done * CHANNELS)..(frames * CHANNELS);
            let Some(result) = shared.write_if_admitted(generation, gate, |gain| {
                let samples = if gain == UNITY {
                    &input[pending]
                } else {
                    if scaled_gain != gain {
                        apply_gain(&input[pending.clone()], &mut scaled[pending.clone()], gain);
                        scaled_gain = gain;
                    }
                    &scaled[pending]
                };
                output.writei_s16(samples, frames - done)
            }) else {
                break;
            };
            let written = finish_io(output, result).map_err(|e| ("запись ALSA", e))?;
            if written != 0 {
                queued_gate = Some(gate);
                done += written;
            }
        }
    }
    Ok(())
}

fn finish_io(pcm: &mut Pcm, result: Result<usize, i32>) -> Result<usize, i32> {
    match result {
        Ok(0) => (),
        Ok(n) => return Ok(n),
        Err(e) if e == -libc::EAGAIN => (),
        Err(e) => return recover(pcm, e),
    }
    match pcm.wait(WAIT_MS) {
        Ok(_) => Ok(0),
        Err(e) => recover(pcm, e),
    }
}

fn recover(pcm: &mut Pcm, err: i32) -> Result<usize, i32> {
    match -err {
        libc::EINTR | libc::EAGAIN => Ok(0),
        libc::EPIPE => pcm.recover(err, true).map(|()| 0),
        // snd_pcm_recover может ждать resume неограниченно при suspend.
        libc::ESTRPIPE => pcm.prepare().map(|()| 0),
        _ => Err(err),
    }
}

fn apply_gain(input: &[i16], output: &mut [i16], gain: [f32; CHANNELS]) {
    if gain == [0.0; CHANNELS] {
        output.fill(0);
        return;
    }
    for (src, dst) in input
        .as_chunks::<CHANNELS>()
        .0
        .iter()
        .zip(output.as_chunks_mut::<CHANNELS>().0)
    {
        for channel in 0..CHANNELS {
            let sample = f32::from(i16::from_le(src[channel]));
            dst[channel] = ((sample * gain[channel]).round() as i16).to_le();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bluetooth_pcm_accepts_only_explicit_device_addresses() {
        assert_eq!(
            capture_name("AA:bb:00:12:03:ff").as_deref(),
            Some("bluealsa:DEV=AA:bb:00:12:03:ff,PROFILE=a2dp"),
        );
        for invalid in [
            "",
            "00:00:00:00:00:0g",
            "00-00-00-00-00-00",
            "00:00:00:00:00:00,DEV=other",
        ] {
            assert!(capture_name(invalid).is_none());
        }
    }

    #[test]
    fn bluetooth_pcm_station_gain_scales_samples_and_mute_writes_exact_zeros() {
        let decoded = [12000_i16.to_le(), (-12000_i16).to_le()];
        let mut output = [0; 2];
        for (station, expected) in [(0.5, [6000, -6000]), (0.125, [1500, -1500]), (0.0, [0, 0])] {
            apply_gain(&decoded, &mut output, [station; CHANNELS]);
            assert_eq!(output.map(i16::from_le), expected);
        }
        apply_gain(&decoded, &mut output, [0.0, 1.0]);
        assert_eq!(output.map(i16::from_le), [0, -12000]);
    }

    #[test]
    fn bluetooth_pcm_rejects_revoked_frames_after_gate_reopens_or_generation_changes() {
        let (events, mut rx) = mpsc::unbounded_channel();
        let worker = Worker {
            shared: Arc::new(Shared::new()),
            events,
            terminated: watch::channel(true).1,
        };
        let mut sink = Vec::new();
        let mut write = |gain: [f32; CHANNELS]| {
            let input = [200_i16.to_le(); 2];
            let mut output = [0; 2];
            apply_gain(&input, &mut output, gain);
            sink.push(output.map(i16::from_le));
        };
        worker.set_station_gain(0.5);
        worker.start(5, String::new());
        worker.set_phone_gain([1.0, 0.0]);
        let (gate, _) = worker.shared.gate(5).unwrap();
        assert!(
            worker
                .shared
                .write_if_admitted(5, gate, &mut write)
                .is_none()
        );
        worker.set_admitted(true);
        assert!(
            worker
                .shared
                .write_if_admitted(5, gate, &mut write)
                .is_some()
        );
        worker.set_admitted(false);
        worker.set_admitted(true);
        assert!(
            worker
                .shared
                .write_if_admitted(5, gate, &mut write)
                .is_none()
        );
        let (new_gate, _) = worker.shared.gate(5).unwrap();
        worker.stop();
        assert_eq!(rx.try_recv().unwrap(), Event::Stopped(5));
        assert!(
            worker
                .shared
                .write_if_admitted(5, new_gate, &mut write)
                .is_none()
        );
        worker.start(6, String::new());
        worker.set_admitted(true);
        assert!(
            worker
                .shared
                .write_if_admitted(5, new_gate, &mut write)
                .is_none()
        );
        let (new_gate, _) = worker.shared.gate(6).unwrap();
        assert!(
            worker
                .shared
                .write_if_admitted(6, new_gate, &mut write)
                .is_some()
        );
        assert_eq!(sink, [[100, 0], [100, 100]]);
    }

    #[test]
    fn bluetooth_pcm_activity_needs_frames_and_only_reports_edges() {
        let start = Instant::now();
        let mut activity = Activity::AwaitingFirstFrame(start);
        assert_eq!(activity.observe(7, false, start), None);
        let Some(Event::Activity(7, first)) = activity.observe(7, true, start) else {
            panic!("first PCM frames must start a playback episode");
        };
        assert_eq!(activity.observe(7, true, start), None);
        assert_eq!(activity.observe(7, false, start + IDLE_AFTER / 2), None);
        assert_eq!(
            activity.observe(7, false, start + IDLE_AFTER),
            Some(Event::Idle(7))
        );
        assert_eq!(activity.observe(7, false, start + IDLE_AFTER), None);
        assert!(matches!(
            activity.observe(7, true, start + IDLE_AFTER),
            Some(Event::Activity(7, next)) if next > first
        ));
    }

    #[test]
    fn bluetooth_pcm_reports_initial_idle_once_before_first_frames() {
        let start = Instant::now();
        let mut activity = Activity::AwaitingFirstFrame(start);
        let first_idle = start + IDLE_AFTER;
        assert_eq!(activity.observe(7, false, start + IDLE_AFTER / 2), None);
        assert_eq!(activity.observe(7, false, first_idle), Some(Event::Idle(7)));
        assert_eq!(activity.observe(7, false, first_idle + IDLE_AFTER), None);
        assert!(matches!(
            activity.observe(7, true, first_idle + IDLE_AFTER),
            Some(Event::Activity(7, _))
        ));
    }
}
