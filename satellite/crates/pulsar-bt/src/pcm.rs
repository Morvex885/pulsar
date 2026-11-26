//! Вывод входящего A2DP: один поток ALSA на все поколения сеанса.

use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use pulsar_alsa::{Format, HwRequest, Pcm, Stream};
use pulsar_core::model::Model;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

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
}

pub(super) struct Probe {
    status: watch::Receiver<ProbeState>,
    cancelled: bool,
}

enum ProbeState {
    Pending,
    Ready,
    Failed(String),
}

struct Request {
    generation: u64,
    // Забирается потоком один раз, но поколение остаётся в затворе до отзыва.
    address: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FirstFrameDecision {
    Waiting,
    Accepted(u64),
    Rejected,
}

struct Control {
    request: Option<Request>,
    shutdown: bool,
    admitted: bool,
    gate: u64,
    first: Option<(u64, FirstFrameDecision)>,
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
                first: None,
                station_gain: 1.0,
                phone_gain: UNITY,
                gain: UNITY,
            }),
            wake: Condvar::new(),
        }
    }

    fn stopping(&self) -> bool {
        self.control.lock().unwrap().shutdown
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

    fn hold_first(&self, generation: u64, order: u64) -> bool {
        let mut ctl = self.control.lock().unwrap();
        if ctl.generation() != Some(generation) {
            return false;
        }
        ctl.first = Some((order, FirstFrameDecision::Waiting));
        true
    }

    fn first_decision(&self, generation: u64, order: u64) -> FirstFrameDecision {
        let ctl = self.control.lock().unwrap();
        if ctl.generation() == Some(generation)
            && let Some((token, decision)) = ctl.first
            && token == order
            && !matches!(decision, FirstFrameDecision::Accepted(gate) if ctl.gate != gate)
        {
            decision
        } else {
            FirstFrameDecision::Rejected
        }
    }

    fn admitted_gain(&self, generation: u64, gate: u64) -> Option<[f32; CHANNELS]> {
        let ctl = self.control.lock().unwrap();
        (ctl.generation() == Some(generation) && ctl.gate == gate && ctl.admitted)
            .then_some(ctl.gain)
    }

    fn write_if_admitted<T>(
        &self,
        generation: u64,
        gate: u64,
        write: impl FnOnce([f32; CHANNELS]) -> T,
    ) -> Option<T> {
        // Единственный поток резервирует одну запись и снимок громкости под замком.
        // Даже неблокирующий writei может застрять на семафоре dmix. Уже
        // зарезервированную запись не прервать, но она не удерживает затвор.
        self.admitted_gain(generation, gate).map(write)
    }
}

impl Worker {
    pub(super) fn new() -> (Self, UnboundedReceiver<Event>) {
        let (events, rx) = mpsc::unbounded_channel();
        let worker = Self {
            shared: Arc::new(Shared::new()),
            events,
        };
        (worker, rx)
    }

    pub(super) fn launch(&self, model: &'static Model) -> io::Result<Probe> {
        self.launch_with(
            move |shared| open_output_if(model, || !shared.stopping()).map(drop),
            move |shared, events| run(model, shared, events),
        )
    }

    fn launch_with(
        &self,
        probe: impl FnOnce(&Shared) -> Result<(), PcmError> + Send + 'static,
        run: impl FnOnce(&Shared, &UnboundedSender<Event>) + Send + 'static,
    ) -> io::Result<Probe> {
        // Повторный запуск возможен только после выхода прежнего потока.
        self.shared.control.lock().unwrap().shutdown = false;
        let (status_tx, status) = watch::channel(ProbeState::Pending);
        let shared = Arc::clone(&self.shared);
        let events = self.events.clone();
        std::thread::Builder::new()
            .name("bluetooth-pcm".into())
            .spawn(move || {
                // Тот же выход и формат, что у сеанса. Закрываем PCM до ответа и не пишем звук.
                if shared.stopping() {
                    return;
                }
                let result = probe(&shared);
                if shared.stopping() {
                    return;
                }
                match result {
                    Ok(()) => {
                        status_tx.send_replace(ProbeState::Ready);
                        run(&shared, &events);
                    }
                    Err((op, errno)) => {
                        status_tx.send_replace(ProbeState::Failed(format!(
                            "{op}: {}",
                            pulsar_alsa::strerror(errno)
                        )));
                    }
                }
            })?;
        Ok(Probe {
            status,
            cancelled: false,
        })
    }

    pub(super) fn signal_stop(&self) {
        {
            let mut ctl = self.shared.control.lock().unwrap();
            ctl.admit(false);
            ctl.request = None;
            ctl.first = None;
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
            ctl.first = None;
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

    pub(super) fn reconcile_admission(
        &self,
        generation: u64,
        admitted: bool,
        first: Option<(u64, bool)>,
    ) {
        let mut ctl = self.shared.control.lock().unwrap();
        if ctl.generation() != Some(generation) {
            return;
        }
        ctl.admit(admitted);
        if let Some((order, accepted)) = first
            && ctl.first == Some((order, FirstFrameDecision::Waiting))
        {
            let decision = if accepted && admitted {
                FirstFrameDecision::Accepted(ctl.gate)
            } else {
                FirstFrameDecision::Rejected
            };
            ctl.first = Some((order, decision));
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

impl Probe {
    pub(super) fn cancel(&mut self, worker: &Worker) {
        self.cancelled = true;
        worker.signal_stop();
    }

    pub(super) fn retire(mut self, worker: &Worker) {
        self.cancel(worker);
        // При отказе от Bluetooth актор сервиса не создаётся. Отдельный актор
        // дожидается закрытия ALSA, но не задерживает инициализацию контроллера.
        pulsar_core::actors::spawn_actor("bluetooth PCM shutdown", async move {
            self.wait_finished().await;
        });
    }

    pub(super) fn finished(&self) -> bool {
        self.status.has_changed().is_err()
    }

    pub(super) fn ready(&self) -> bool {
        !self.cancelled && !self.finished() && matches!(*self.status.borrow(), ProbeState::Ready)
    }

    pub(super) async fn wait_ready(&mut self) -> io::Result<()> {
        match self
            .status
            .wait_for(|s| !matches!(s, ProbeState::Pending))
            .await
        {
            Ok(state) => match &*state {
                ProbeState::Ready => Ok(()),
                ProbeState::Failed(reason) => Err(io::Error::other(reason.clone())),
                ProbeState::Pending => unreachable!(),
            },
            Err(_) => Err(io::Error::other("PCM worker stopped during output probe")),
        }
    }

    pub(super) async fn wait_finished(&mut self) {
        while self.status.changed().await.is_ok() {}
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
            // Короткий сигнал может закончиться, не заполнив целый период.
            Stream::Playback => pcm.set_sw_params(1, period)?,
        }
        pcm.prepare()
    })();
    result.map_err(|e| (context, e))
}

fn open_output_if(model: &Model, still_needed: impl Fn() -> bool) -> Result<Option<Pcm>, PcmError> {
    if !still_needed() {
        return Ok(None);
    }
    let mut output = pulsar_alsa::open_playback_nonblock(model).map_err(|e| ("выход ALSA", e))?;
    // Если отмена пришла во время snd_pcm_open, закрываем выход без настройки.
    if !still_needed() {
        return Ok(None);
    }
    configure(&mut output, Stream::Playback, "формат выхода ALSA")?;
    Ok(Some(output))
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
    let Some(mut output) = open_output_if(model, || shared.gate(generation).is_some())? else {
        return Ok(());
    };
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

struct BufferedFirst {
    order: u64,
    slot: usize,
    frames: usize,
    idle: bool,
}

fn transfer(
    capture: &mut Pcm,
    output: &mut Pcm,
    shared: &Shared,
    events: &UnboundedSender<Event>,
    generation: u64,
) -> Result<(), PcmError> {
    // Пока решается допуск, второй буфер используется для захвата без
    // накопления очереди; при записи он же нужен для пересчёта громкости.
    let mut buffers = [[0; FRAMES * CHANNELS]; 2];
    let mut first = None::<BufferedFirst>;
    let mut activity = Activity::AwaitingFirstFrame(Instant::now());
    let mut queued_gate = None;
    let mut idle_after_write = None;
    while let Some((gate, admitted)) = shared.gate(generation) {
        if queued_gate.is_some_and(|previous| previous != gate) {
            output.drop_frames().map_err(|e| ("сброс выхода ALSA", e))?;
            output
                .prepare()
                .map_err(|e| ("подготовка выхода ALSA", e))?;
            queued_gate = None;
        }
        if let Some(pending) = &first {
            let decision = shared.first_decision(generation, pending.order);
            // Сначала сбросить очередь предыдущего допуска в начале цикла.
            if matches!(decision, FirstFrameDecision::Accepted(accepted) if accepted != gate) {
                continue;
            }
            if decision != FirstFrameDecision::Waiting {
                let pending = first.take().unwrap();
                let written = if let FirstFrameDecision::Accepted(gate) = decision {
                    let written = write_frames(
                        output,
                        shared,
                        generation,
                        gate,
                        &mut buffers,
                        (pending.slot, pending.frames),
                    )?;
                    if written {
                        queued_gate = Some(gate);
                    }
                    written
                } else {
                    false
                };
                if pending.idle {
                    if written {
                        // Не закрывать затвор, пока только что записанный
                        // короткий звук ещё находится в очереди ALSA.
                        idle_after_write = Some(Instant::now() + IDLE_AFTER);
                    } else if events.send(Event::Idle(generation)).is_err() {
                        return Ok(());
                    }
                }
                continue;
            }
        }
        let slot = first.as_ref().map_or(0, |pending| pending.slot ^ 1);
        let result = capture.readi(&mut buffers[slot], FRAMES);
        let after_idle = matches!(activity, Activity::Idle);
        let frames = finish_io(capture, result).map_err(|e| ("чтение A2DP", e))?;
        let now = Instant::now();
        if idle_after_write.is_some_and(|at| frames != 0 || now >= at) {
            idle_after_write = None;
            if events.send(Event::Idle(generation)).is_err() {
                return Ok(());
            }
        }
        if let Some(event) = activity.observe(generation, frames != 0, now) {
            match event {
                Event::Activity(_, order) => {
                    // Новый фронт заменяет старый буфер, даже если его допуск
                    // пришёл во время чтения: нельзя записать его за новый сеанс.
                    let replaced = first.take().is_some();
                    if replaced && events.send(Event::Idle(generation)).is_err() {
                        return Ok(());
                    }
                    if (!admitted || replaced || after_idle) && shared.hold_first(generation, order)
                    {
                        first = Some(BufferedFirst {
                            order,
                            slot,
                            frames,
                            idle: false,
                        });
                    }
                }
                Event::Idle(_) => {
                    if let Some(pending) = &mut first {
                        // Даже короткий звук из одного буфера ждёт решения.
                        pending.idle = true;
                        continue;
                    }
                }
                _ => {}
            }
            if events.send(event).is_err() {
                return Ok(());
            }
        }
        if !admitted || first.is_some() || frames == 0 {
            continue;
        }
        if write_frames(
            output,
            shared,
            generation,
            gate,
            &mut buffers,
            (slot, frames),
        )? {
            queued_gate = Some(gate);
        }
    }
    Ok(())
}

fn write_frames(
    output: &mut Pcm,
    shared: &Shared,
    generation: u64,
    gate: u64,
    buffers: &mut [[i16; FRAMES * CHANNELS]; 2],
    (slot, frames): (usize, usize),
) -> Result<bool, PcmError> {
    let [a, b] = buffers;
    let (input, scaled) = if slot == 0 { (a, b) } else { (b, a) };
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
        if let Ok(n) = result
            && n != 0
        {
            done += n;
        } else if shared.admitted_gain(generation, gate).is_some() {
            finish_io(output, result).map_err(|e| ("запись ALSA", e))?;
        } else {
            // После отзыва не ждём и не восстанавливаем старый вывод:
            // очередь сбросит transfer при смене затвора или session при выходе.
            break;
        }
    }
    Ok(done != 0)
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

    #[tokio::test(start_paused = true)]
    async fn bluetooth_rejected_pcm_probe_is_cancelled_and_tracked_until_output_closes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (worker, _) = Worker::new();
                let (opened, started) = tokio::sync::oneshot::channel();
                let (close, closing) = std::sync::mpsc::channel();
                let (after_close, closed) = tokio::sync::oneshot::channel();
                let mut probe = worker
                    .launch_with(
                        move |shared| {
                            opened.send(()).unwrap();
                            closing.recv().unwrap();
                            after_close.send(shared.stopping()).unwrap();
                            Ok(())
                        },
                        |_, _| panic!("cancelled probe must not run a session"),
                    )
                    .unwrap();
                started.await.unwrap();
                assert!(
                    tokio::time::timeout(Duration::ZERO, probe.wait_ready())
                        .await
                        .is_err()
                );
                let mut status = probe.status.clone();
                probe.retire(&worker);
                assert!(worker.shared.stopping());
                assert!(!pulsar_core::actors::join_all(Duration::ZERO).await);

                close.send(()).unwrap();
                assert!(closed.await.unwrap());
                assert!(status.changed().await.is_err());
            })
            .await;
    }

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
    fn bluetooth_pcm_preempts_while_reserved_write_is_blocked() {
        let (worker, _) = Worker::new();
        worker.start(5, String::new());
        worker.set_admitted(true);
        let shared = &worker.shared;
        let (old_gate, _) = shared.gate(5).unwrap();
        std::thread::scope(|scope| {
            let (entered, in_write) = std::sync::mpsc::channel();
            let (release, blocked) = std::sync::mpsc::channel();
            let writer = scope.spawn(move || {
                let written = shared.write_if_admitted(5, old_gate, |gain| {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                    assert_eq!(gain, UNITY);
                    1
                });
                assert_eq!(written, Some(1));
                assert!(
                    shared
                        .write_if_admitted(5, old_gate, |_| panic!("запись после отзыва"))
                        .is_none()
                );
            });
            in_write.recv().unwrap();
            // Подтверждение пришло из записи, которая не выйдет до release.
            // try_lock ловит регрессию без зависания и таймаутов.
            assert!(shared.control.try_lock().is_ok());
            worker.set_station_gain(0.0);
            worker.set_admitted(false);
            worker.set_admitted(true);
            let (new_gate, _) = shared.gate(5).unwrap();
            assert_ne!(old_gate, new_gate);
            release.send(()).unwrap();
            writer.join().unwrap();
            assert_eq!(
                shared.write_if_admitted(5, new_gate, |gain| gain),
                Some([0.0; CHANNELS])
            );
        });
    }

    #[test]
    fn bluetooth_pcm_first_buffer_needs_its_own_generation_gate_and_episode_grant() {
        let (worker, _) = Worker::new();
        let shared = &worker.shared;
        worker.start(5, String::new());
        let (gate, _) = shared.gate(5).unwrap();
        assert!(shared.hold_first(5, 10));
        assert_eq!(shared.first_decision(5, 10), FirstFrameDecision::Waiting);
        worker.reconcile_admission(5, true, Some((9, true)));
        assert_eq!(shared.first_decision(5, 10), FirstFrameDecision::Waiting);
        worker.reconcile_admission(5, true, Some((10, true)));
        assert_eq!(
            shared.first_decision(5, 10),
            FirstFrameDecision::Accepted(gate)
        );
        assert_eq!(
            shared.write_if_admitted(5, gate, |_| [123_i16, -123]),
            Some([123, -123])
        );

        assert!(shared.hold_first(5, 11));
        assert_eq!(shared.first_decision(5, 10), FirstFrameDecision::Rejected);
        worker.reconcile_admission(5, true, Some((11, false)));
        worker.reconcile_admission(5, true, Some((11, true)));
        assert_eq!(shared.first_decision(5, 11), FirstFrameDecision::Rejected);
        assert!(shared.hold_first(5, 12));
        worker.reconcile_admission(5, true, Some((12, true)));
        worker.set_admitted(false);
        worker.set_admitted(true);
        assert_eq!(shared.first_decision(5, 12), FirstFrameDecision::Rejected);

        assert!(shared.hold_first(5, 13));
        worker.set_admitted(false);
        assert_eq!(shared.first_decision(5, 13), FirstFrameDecision::Waiting);
        let (new_gate, _) = shared.gate(5).unwrap();
        worker.reconcile_admission(5, true, Some((13, true)));
        assert_eq!(
            shared.first_decision(5, 13),
            FirstFrameDecision::Accepted(new_gate)
        );
        assert!(shared.hold_first(5, 14));
        worker.start(6, String::new());
        worker.reconcile_admission(5, true, Some((14, true)));
        assert_eq!(shared.first_decision(5, 14), FirstFrameDecision::Rejected);
        assert!(
            shared
                .write_if_admitted(5, new_gate, |_| [123_i16, -123])
                .is_none()
        );
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
