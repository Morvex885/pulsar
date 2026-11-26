use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use log::warn;
use pulsar_core::{Timers, actors, model::Model};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, timeout};

use crate::bus::{Bus, Candidate};
use crate::pcm::{Event as PcmEvent, Worker};
use crate::policy::Policy;
use crate::stock::{Stock, plugin_present};
use crate::{BtAction, Playback};

const PROBE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);

pub enum BtEvent {
    Changed,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BtState {
    pub enabled: bool,
    pub pairing: bool,
    pub status: String,
    pub playback: Option<Playback>,
    pub admitted: bool,
    pub starts: u64,
    /// Идентификатор принятого телефона, не меняется при перезапуске PCM.
    pub connection: Option<u64>,
}

impl BtState {
    pub fn connected(&self) -> bool {
        self.connection.is_some()
    }
}

enum Cmd {
    Enable(bool),
    Avr(BtAction),
    Preempt,
    Admit(u64),
}

struct Shared {
    state: RefCell<BtState>,
    requested: Cell<bool>,
    preempt_before: Cell<u64>,
    pcm: Worker,
}

impl Shared {
    fn eligible(&self, starts: u64) -> bool {
        self.requested.get() && starts >= self.preempt_before.get()
    }
}

#[derive(Clone)]
pub struct BtHandle {
    shared: Rc<Shared>,
    cmds: UnboundedSender<Cmd>,
}

impl BtHandle {
    pub fn state(&self) -> BtState {
        let mut state = self.shared.state.borrow().clone();
        state.admitted &= self.shared.eligible(state.starts);
        state
    }

    pub fn playing(&self) -> bool {
        let state = self.shared.state.borrow();
        state.admitted && self.shared.eligible(state.starts)
    }

    pub fn set_enabled(&self, on: bool) {
        self.shared.requested.set(on);
        if !on {
            self.shared.pcm.stop();
        }
        let _ = self.cmds.send(Cmd::Enable(on));
    }

    pub fn avr(&self, action: BtAction) {
        let _ = self.cmds.send(Cmd::Avr(action));
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared.pcm.set_station_gain(gain);
    }

    pub fn preempt_before(&self, order: u64) {
        if order <= self.shared.preempt_before.get() {
            return;
        }
        self.shared.preempt_before.set(order);
        let state = self.shared.state.borrow();
        if state.admitted && state.starts < order {
            self.shared.pcm.set_admitted(false);
        }
        let _ = self.cmds.send(Cmd::Preempt);
    }

    pub fn admit(&self, starts: u64) {
        let _ = self.cmds.send(Cmd::Admit(starts));
    }
}

pub async fn spawn(
    model: &'static Model,
    enabled: bool,
) -> Option<(BtHandle, UnboundedReceiver<BtEvent>)> {
    let (stock, owned, supported) = match tokio::task::spawn_blocking(|| {
        Ok::<_, std::io::Error>(Stock::load()?.map(|(stock, owned)| {
            let supported = Path::new("/sys/class/bluetooth/hci0").exists() && plugin_present();
            (stock, owned, supported)
        }))
    })
    .await
    {
        Ok(Ok(station)) => station?,
        Ok(Err(e)) => {
            warn!("bluetooth: cannot verify stock ownership: {e}");
            return None;
        }
        Err(e) => {
            warn!("bluetooth: cannot inspect stock ownership: {e}");
            return None;
        }
    };
    if !supported {
        recover_stock(stock, owned);
        return None;
    }
    let bus = match timeout(PROBE, Bus::connect()).await {
        Ok(Ok(bus)) => bus,
        Ok(Err(e)) => {
            warn!("bluetooth: capability probe: {}", error_name(&e));
            recover_stock(stock, owned);
            return None;
        }
        Err(_) => {
            warn!("bluetooth: capability probe timed out");
            recover_stock(stock, owned);
            return None;
        }
    };
    let flags = bus.flags().expect("capability probe checked adapter flags");
    let (pcm, pcm_rx) = match Worker::spawn(model).await {
        Ok(pair) => pair,
        Err(e) => {
            warn!("bluetooth: cannot start PCM worker: {e}");
            recover_stock(stock, owned);
            return None;
        }
    };
    pcm.set_station_gain(0.0);
    if let Err(e) = stock.take(flags).await {
        warn!("bluetooth: cannot take stock player: {e}");
        pcm.shutdown().await;
        // Init-скрипт мог остановить player прежде, чем вернуть ошибку.
        recover_stock(stock, true);
        return None;
    }
    let policy = Policy::restore(enabled, bus.paired());
    let (events, ev_rx) = unbounded_channel();
    let (cmds, cmd_rx) = unbounded_channel();
    let shared = Rc::new(Shared {
        state: RefCell::new(BtState {
            enabled,
            status: if enabled { "ready" } else { "off" }.into(),
            ..BtState::default()
        }),
        requested: Cell::new(enabled),
        preempt_before: Cell::new(0),
        pcm,
    });
    let svc = Service {
        shared: shared.clone(),
        stock,
        events,
        policy,
        bus: Some(bus),
        reject_at: HashMap::new(),
        pcm_generation: None,
        pcm_error: None,
        flag_error: None,
        visibility_target: None,
        retry_at: Instant::now(),
        frames: FrameActivity::default(),
        granted: None,
        pcm_ready: false,
        pcm_retry_at: None,
    };
    actors::spawn_actor("bluetooth", svc.run(cmd_rx, pcm_rx));
    Some((BtHandle { shared, cmds }, ev_rx))
}

fn recover_stock(stock: Stock, outstanding: bool) {
    if outstanding {
        actors::spawn_actor("bluetooth stock restore", retry_stock_restore(stock));
    }
}

async fn retry_stock_restore(stock: Stock) {
    let mut retry_at = Instant::now();
    loop {
        tokio::select! {
            biased;
            () = actors::shutdown_requested() => return,
            result = async {
                tokio::time::sleep_until(retry_at).await;
                stock.restore().await
            } => match result {
                Ok(()) => return,
                Err(e) => warn!("bluetooth: cannot restore stock ownership: {e}"),
            },
        }
        retry_at = Instant::now() + RETRY;
    }
}

#[derive(Default)]
struct FrameActivity {
    active: bool,
    edge: Option<FrameEdge>,
}

enum FrameEdge {
    Idle,
    Active(u64),
}

impl FrameActivity {
    fn start(&mut self, order: u64) {
        // Новое открытие PCM не означает новый старт уже идущего потока.
        if !self.active {
            self.active = true;
            self.edge = Some(FrameEdge::Active(order));
        }
    }

    fn idle(&mut self) {
        self.active = false;
        self.edge = Some(FrameEdge::Idle);
    }

    fn observe(&mut self, policy: &mut Policy, avr: Option<Playback>) {
        // Кадры, уже учтённые через AVRCP, не создают фронт при исчезновении статуса.
        let edge = self.edge.take();
        let (phase, order) = match (avr, edge) {
            (Some(phase), _) => (Some(phase), None),
            (None, Some(FrameEdge::Active(order))) => (Some(Playback::Playing), Some(order)),
            (None, Some(FrameEdge::Idle)) => (None, None),
            (None, None) => return,
        };
        policy.observe_playback(policy.generation, phase, order);
    }
}

struct Service {
    shared: Rc<Shared>,
    stock: Stock,
    events: UnboundedSender<BtEvent>,
    policy: Policy,
    bus: Option<Bus>,
    reject_at: HashMap<String, Instant>,
    pcm_generation: Option<u64>,
    pcm_error: Option<&'static str>,
    flag_error: Option<&'static str>,
    visibility_target: Option<bool>,
    retry_at: Instant,
    frames: FrameActivity,
    granted: Option<u64>,
    pcm_ready: bool,
    pcm_retry_at: Option<Instant>,
}

impl Service {
    async fn run(
        mut self,
        mut cmds: UnboundedReceiver<Cmd>,
        mut pcm_rx: UnboundedReceiver<PcmEvent>,
    ) {
        let mut timers = Timers::new();
        let mut armed = None;
        let mut worker_failed = false;
        loop {
            if self.reconcile() {
                self.avr(BtAction::Pause).await;
            }
            let deadline = [Some(self.retry_at), self.policy.deadline, self.pcm_retry_at]
                .into_iter()
                .flatten()
                .min()
                .expect("retry timer");
            if armed != Some(deadline) {
                timers.set_at((), deadline);
                armed = Some(deadline);
            }
            tokio::select! {
                biased;
                () = timers.next() => {
                    armed = None;
                    if self.retry_at <= Instant::now() {
                        self.maintain().await;
                    }
                },
                cmd = cmds.recv() => match cmd {
                    Some(cmd) => self.command(cmd).await,
                    None => break,
                },
                res = async {
                    let bus = self.bus.as_mut().expect("guarded bus");
                    let old = bus.flags();
                    let result = bus.next().await;
                    (old, result, bus.flags())
                }, if self.bus.is_some() => {
                    let (old, result, flags) = res;
                    if result.is_err() || flags.is_none() {
                        self.lost_bus();
                    } else if flags != old && flags != Some([self.visibility_open(); 2]) {
                        self.retry_at = Instant::now();
                    }
                },
                ev = pcm_rx.recv() => match ev {
                    Some(ev) => self.on_pcm(ev),
                    None => {
                        self.policy.release();
                        self.reset_session();
                        self.pcm_error = Some("PCM worker stopped");
                        self.publish();
                        worker_failed = true;
                        break;
                    }
                },
            }
        }
        self.shared.pcm.shutdown().await;
        if worker_failed {
            retry_stock_restore(self.stock).await;
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Enable(on) => {
                let paired = self.bus.as_ref().filter(|_| on).map(Bus::paired);
                self.policy.set_enabled(on, Instant::now(), paired);
                if !on {
                    self.reset_session();
                }
                self.retry_at = Instant::now();
            }
            // Порог уже установлен в handle; сообщение только будит актор.
            Cmd::Preempt => {}
            Cmd::Admit(starts) => {
                if self.policy.starts == starts
                    && self.policy.admitted
                    && self.shared.eligible(starts)
                {
                    self.granted = Some(starts);
                }
            }
            Cmd::Avr(action) => self.avr(action).await,
        }
    }

    async fn avr(&self, action: BtAction) {
        if let Some(bus) = &self.bus
            && let Some(owner) = self.policy.owner.as_deref()
            && let Some(player) = bus
                .candidates()
                .find(|c| c.device == owner)
                .and_then(|c| c.player)
            && let Err(e) = bus.avr(player, action.method()).await
        {
            warn!("bluetooth: AVRCP {}: {}", action.method(), error_name(&e));
        }
    }

    fn lost_bus(&mut self) {
        self.bus = None;
        self.policy.release();
        self.policy.deadline = None;
        self.reset_session();
        self.flag_error = None;
        self.visibility_target = None;
        self.reject_at.clear();
        self.retry_at = Instant::now() + RETRY;
    }

    fn reset_session(&mut self) {
        self.shared.pcm.stop();
        self.pcm_generation = None;
        self.pcm_ready = false;
        self.pcm_error = None;
        self.pcm_retry_at = None;
        self.frames = FrameActivity::default();
        self.granted = None;
    }

    fn visibility_open(&self) -> bool {
        self.policy.deadline.is_some() && self.shared.requested.get()
    }

    fn reconcile(&mut self) -> bool {
        self.policy.expire(Instant::now());
        if self.pcm_retry_at.is_some_and(|at| Instant::now() >= at) {
            self.pcm_retry_at = None;
            self.pcm_error = None;
        }
        if let Some(bus) = &self.bus {
            self.policy.observe_pairs(&bus.paired());
            if let Some(owner) = self.policy.owner.as_deref()
                && (!self.policy.permits(owner) || selected(bus, &self.policy).is_none())
            {
                self.policy.release();
                self.reset_session();
            }
        }
        if let Some(bus) = &self.bus {
            if self.shared.requested.get()
                && let Some(device) = self.policy.choose(
                    bus.candidates()
                        .filter(|c| !c.address.is_empty())
                        .map(|c| (c.device, c.sequence)),
                )
            {
                self.policy.select(device.to_owned());
            }
            if let Some(c) = selected(bus, &self.policy) {
                self.frames.observe(&mut self.policy, c.phase);
                if self.pcm_generation != Some(self.policy.generation)
                    && self.pcm_error.is_none()
                    && self.shared.requested.get()
                {
                    self.pcm_ready = false;
                    self.shared
                        .pcm
                        .start(self.policy.generation, c.address.to_owned());
                    self.pcm_generation = Some(self.policy.generation);
                }
                self.shared.pcm.set_phone_gain(c.phone_gain);
            }
            if self.visibility_target != Some(self.visibility_open()) {
                self.retry_at = Instant::now();
            }
        }
        let pause = self.policy.preempt_before(self.shared.preempt_before.get());
        if !self.policy.admitted {
            self.granted = None;
        }
        let ready = self.ready_start();
        self.shared
            .pcm
            .set_admitted(ready.is_some() && self.granted == ready);
        self.publish();
        pause
    }

    fn on_pcm(&mut self, ev: PcmEvent) {
        match ev {
            PcmEvent::Activity(g, order) if self.pcm_generation == Some(g) => {
                self.frames.start(order);
            }
            PcmEvent::Idle(g) if self.pcm_generation == Some(g) => {
                self.frames.idle();
            }
            PcmEvent::Failed(g, reason) if self.pcm_generation == Some(g) => {
                warn!("bluetooth: PCM: {reason}");
                self.pcm_generation = None;
                self.pcm_ready = false;
                self.pcm_error = Some("PCM output unavailable");
                // Восстановление выхода не делает старый поток новым источником.
                self.policy.generation += 1;
                self.pcm_retry_at = Some(Instant::now() + RETRY);
            }
            PcmEvent::Started(g) if self.pcm_generation == Some(g) => self.pcm_ready = true,
            PcmEvent::Stopped(g) if self.pcm_generation == Some(g) => {
                self.pcm_ready = false;
                self.pcm_generation = None;
                self.policy.generation += 1;
            }
            _ => {}
        }
    }

    async fn maintain(&mut self) {
        let now = Instant::now();
        let open = self.visibility_open();
        self.retry_at = now + RETRY;
        let Some(bus) = &self.bus else {
            if let Ok(Ok(bus)) = timeout(PROBE, Bus::connect()).await {
                self.bus = Some(bus);
                self.retry_at = Instant::now();
            }
            return;
        };
        if visibility_needs_write(self.visibility_target, bus.flags(), open) {
            self.visibility_target = Some(open);
            match bus.visibility(open).await {
                Ok(()) => self.flag_error = None,
                Err(e) => {
                    warn!("bluetooth: pairing flags: {}", error_name(&e));
                    self.flag_error = Some("cannot set pairing flags");
                }
            }
        } else {
            self.flag_error = None;
        }
        self.reject_at.retain(|device, _| {
            self.policy.owner.as_deref() != Some(device.as_str())
                && bus.candidates().any(|c| c.device == device)
        });
        // Не задерживать обработку новых сигналов за серией D-Bus вызовов к нескольким телефонам.
        if self.policy.deadline.is_none()
            && let Some(device) = next_rejection(
                bus.candidates().map(|c| c.device),
                self.policy.owner.as_deref(),
                &self.reject_at,
                now,
            )
        {
            self.reject_at
                .insert(device.to_owned(), Instant::now() + RETRY);
            if let Err(e) = bus.disconnect(device).await {
                warn!("bluetooth: DisconnectProfile: {}", error_name(&e));
            }
        }
        self.publish();
    }

    fn ready_start(&self) -> Option<u64> {
        self.policy.ready_start(
            self.pcm_ready && self.shared.requested.get(),
            self.shared.preempt_before.get(),
        )
    }

    fn publish(&self) {
        let c = self
            .bus
            .as_ref()
            .and_then(|bus| selected(bus, &self.policy));
        let pairing =
            self.visibility_open() && self.bus.as_ref().and_then(Bus::flags) == Some([true; 2]);
        let error = self.pcm_error.or(self.flag_error).or_else(|| {
            self.bus
                .is_none()
                .then_some("Bluetooth service unavailable")
        });
        let status = if let Some(error) = error {
            format!("error: {error}")
        } else if let Some(c) = &c {
            let suffix = match self.policy.playback {
                Some(Playback::Playing) => " (playing)",
                Some(Playback::Paused) => " (paused)",
                None => "",
            };
            format!("connected: {}{suffix}", c.name)
        } else if pairing {
            "discoverable".into()
        } else if self.policy.enabled {
            "ready".into()
        } else {
            "off".into()
        };
        let mut old = self.shared.state.borrow_mut();
        let ready = self.ready_start();
        let next = BtState {
            enabled: self.policy.enabled,
            pairing,
            status,
            playback: self.policy.playback,
            admitted: ready.is_some(),
            // Контроллер получает фронт только вместе с готовностью выхода.
            starts: ready.unwrap_or(old.starts),
            connection: c.and(self.policy.connection),
        };
        if *old != next {
            *old = next;
            let _ = self.events.send(BtEvent::Changed);
        }
    }
}

fn visibility_needs_write(target: Option<bool>, flags: Option<[bool; 2]>, open: bool) -> bool {
    // Сигналы от предыдущего Set могут ещё не дойти до локального снимка.
    target != Some(open) || flags != Some([open; 2])
}

fn next_rejection<'a>(
    devices: impl Iterator<Item = &'a str>,
    owner: Option<&str>,
    retry_at: &HashMap<String, Instant>,
    now: Instant,
) -> Option<&'a str> {
    devices
        .filter(|&device| Some(device) != owner)
        .filter_map(|device| {
            let sent = retry_at.get(device).copied();
            sent.is_none_or(|at| now >= at).then_some((sent, device))
        })
        .min()
        .map(|(_, device)| device)
}

fn selected<'a>(bus: &'a Bus, policy: &Policy) -> Option<Candidate<'a>> {
    let owner = policy.owner.as_deref()?;
    bus.candidates()
        .find(|c| c.device == owner && !c.address.is_empty())
}

fn error_name(error: &zbus::Error) -> &str {
    match error {
        zbus::Error::MethodError(name, _, _) => name.as_str(),
        zbus::Error::Failure(reason) => reason.as_str(),
        _ => "invalid reply or disconnected bus",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn selected_policy() -> Policy {
        let mut policy = Policy::restore(true, BTreeSet::from(["phone".into()]));
        policy.select("phone".into());
        policy
    }

    #[test]
    fn avr_start_keeps_its_order_while_waiting_for_pcm() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        frames.observe(&mut policy, Some(Playback::Playing));
        let phone_start = policy.starts;
        assert_eq!(policy.ready_start(false, 0), None);
        assert_eq!(policy.ready_start(true, 0), Some(phone_start));

        let music_start = pulsar_core::audio_start_order();
        assert!(phone_start < music_start);
        // Reconcile может отработать до сообщения, если актор ожидал D-Bus.
        assert_eq!(policy.ready_start(true, music_start), None);
        assert!(policy.preempt_before(music_start));
        frames.observe(&mut policy, Some(Playback::Playing));
        assert!(!policy.preempt_before(music_start));
        assert_eq!(policy.ready_start(true, music_start), None);

        frames.observe(&mut policy, Some(Playback::Paused));
        frames.observe(&mut policy, Some(Playback::Playing));
        assert!(policy.ready_start(true, music_start).unwrap() > music_start);
    }

    #[test]
    fn fallback_worker_reopen_does_not_create_a_start_until_idle() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let phone_start = pulsar_core::audio_start_order();
        frames.start(phone_start);
        frames.observe(&mut policy, None);
        assert_eq!(policy.ready_start(true, 0), Some(phone_start));
        let music_start = pulsar_core::audio_start_order();
        assert!(policy.preempt_before(music_start));

        // Первые кадры после повтора открытия ещё принадлежат старому потоку.
        frames.start(pulsar_core::audio_start_order());
        frames.observe(&mut policy, None);
        assert_eq!(policy.starts, phone_start);
        assert_eq!(policy.ready_start(true, music_start), None);

        frames.idle();
        frames.observe(&mut policy, None);
        let next = pulsar_core::audio_start_order();
        frames.start(next);
        frames.observe(&mut policy, None);
        assert_eq!(policy.ready_start(true, music_start), Some(next));
    }

    #[test]
    fn losing_avrcp_status_does_not_reuse_previously_observed_frames() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        frames.observe(&mut policy, Some(Playback::Playing));
        let phone_start = policy.starts;
        frames.start(pulsar_core::audio_start_order());
        frames.observe(&mut policy, Some(Playback::Playing));
        frames.observe(&mut policy, Some(Playback::Paused));
        frames.observe(&mut policy, None);
        assert_eq!(policy.starts, phone_start);
        assert_eq!(policy.ready_start(true, 0), None);

        frames.idle();
        frames.observe(&mut policy, None);
        frames.start(pulsar_core::audio_start_order());
        frames.observe(&mut policy, None);
        assert!(policy.ready_start(true, 0).unwrap() > phone_start);
    }

    #[test]
    fn rejected_profiles_retry_in_due_order_despite_duplicate_pcms() {
        let now = Instant::now();
        let devices = || ["first", "first", "selected", "second"].into_iter();
        let mut deadlines = HashMap::new();
        let next = |deadlines: &HashMap<String, Instant>, now| {
            next_rejection(devices(), Some("selected"), deadlines, now)
        };
        assert_eq!(next(&deadlines, now), Some("first"));
        deadlines.insert("first".into(), now + RETRY);
        // На каждом проходе первая ошибка уже созрела, но не вытесняет вторую.
        assert_eq!(next(&deadlines, now + RETRY), Some("second"));
        deadlines.insert("second".into(), now + RETRY * 2);
        assert_eq!(next(&deadlines, now + RETRY * 2), Some("first"));
        deadlines.insert("first".into(), now + RETRY * 3);
        assert_eq!(next(&deadlines, now + RETRY * 3), Some("second"));
    }

    #[test]
    fn close_is_written_even_when_open_signals_are_still_queued() {
        assert!(visibility_needs_write(Some(true), Some([false; 2]), false));
        assert!(visibility_needs_write(
            Some(false),
            Some([true, false]),
            false
        ));
        assert!(!visibility_needs_write(
            Some(false),
            Some([false; 2]),
            false
        ));
    }
}
