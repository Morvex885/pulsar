use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use log::warn;
use pulsar_core::{Timers, actors, model::Model};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, timeout_at};

use crate::bus::{Bus, Candidate, close_pairing};
use crate::pcm::{Event as PcmEvent, Probe as PcmProbe, Worker};
use crate::policy::Policy;
use crate::stock::{Ownership, Stock, plugin_present};
use crate::{BtAction, Playback};

const PROBE: Duration = Duration::from_secs(2);
const RETRY: Duration = Duration::from_secs(5);

pub enum BtEvent {
    Changed { connection: Option<u64> },
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
    Enable { on: bool, at: Option<u64> },
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
        let was_on = self.shared.requested.replace(on);
        let at = (on && !was_on).then(pulsar_core::audio_start_order);
        if !on {
            self.shared.pcm.stop();
        }
        let _ = self.cmds.send(Cmd::Enable { on, at });
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
    let deadline = Instant::now() + PROBE;
    let (stock, ownership) = match tokio::task::spawn_blocking(Stock::load).await {
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
    if ownership == Ownership::PendingRestore {
        actors::spawn_actor("bluetooth stock restore", retry_stock_restore(stock));
        return None;
    }
    let owned = ownership == Ownership::Owned;
    let mut bus = connect_bus(deadline).await;
    let (pcm, pcm_rx) = Worker::new();
    let mut probe = None;
    pcm.set_station_gain(0.0);
    if bus.is_some() && Instant::now() < deadline {
        match pcm.launch(model) {
            Ok(mut launched) => {
                match timeout_at(deadline, launched.wait_ready()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!("bluetooth: cannot verify PCM output: {e}");
                        launched.cancel(&pcm);
                    }
                    Err(_) => {
                        warn!("bluetooth: PCM output probe timed out");
                        launched.cancel(&pcm);
                    }
                }
                // В том числе отменённый поток жив до возврата из ALSA.
                probe = Some(launched);
            }
            Err(e) => warn!("bluetooth: cannot start PCM worker: {e}"),
        }
    }
    if !owned {
        let flags = if probe.as_ref().is_some_and(PcmProbe::ready) {
            match bus.as_mut() {
                Some(bus) => stock_flags(bus, deadline).await,
                None => None,
            }
        } else {
            None
        };
        let Some(flags) = flags else {
            if let Some(probe) = probe {
                probe.retire(&pcm);
            }
            return None;
        };
        if let Err(e) = stock.take(flags).await {
            warn!("bluetooth: cannot take stock player: {e}");
            if let Some(probe) = probe {
                probe.retire(&pcm);
            }
            // Init-скрипт мог остановить player прежде, чем вернуть ошибку.
            actors::spawn_actor("bluetooth stock restore", retry_stock_restore(stock));
            return None;
        }
    }
    let mut policy = Policy::restore(enabled, Default::default());
    if let Some(bus) = &mut bus {
        seed_startup_pairs(bus, &mut policy, false);
    }
    let (events, ev_rx) = unbounded_channel();
    let (cmds, cmd_rx) = unbounded_channel();
    let shared = Rc::new(Shared {
        state: RefCell::new(BtState::default()),
        requested: Cell::new(enabled),
        preempt_before: Cell::new(0),
        pcm,
    });
    let svc = Service {
        shared: shared.clone(),
        model,
        stock,
        stock_ready: !owned,
        events,
        policy,
        seeded_pairs: bus.is_some(),
        bus,
        probe,
        reject_at: HashMap::new(),
        pcm_generation: None,
        pcm_error: None,
        flag_error: None,
        visibility_target: None,
        retry_at: Instant::now(),
        enable_order: None,
        frames: FrameActivity::default(),
        previous_episode: None,
        granted: None,
        pcm_ready: false,
        pcm_retry_at: None,
    };
    svc.publish();
    actors::spawn_actor("bluetooth", svc.run(cmd_rx, pcm_rx));
    Some((BtHandle { shared, cmds }, ev_rx))
}

async fn stock_flags(bus: &mut Bus, deadline: Instant) -> Option<[bool; 2]> {
    if Instant::now() >= deadline {
        return None;
    }
    // За время проверки PCM адаптер мог измениться. Снимок отката должен
    // учитывать и эти сигналы, не продлевая общий срок проверки возможностей.
    match timeout_at(deadline, bus.settle()).await {
        Ok(Ok(())) => bus.flags(),
        Ok(Err(e)) => {
            warn!(
                "bluetooth: cannot refresh stock adapter flags: {}",
                error_name(&e)
            );
            None
        }
        Err(_) => {
            warn!("bluetooth: stock adapter flags probe timed out");
            None
        }
    }
}

fn seed_startup_pairs(bus: &mut Bus, policy: &mut Policy, seeded: bool) {
    let (known, observed) = bus.finish_startup();
    if seeded {
        policy.observe_pairs(|path| known.contains(path));
    } else {
        policy.seed_known(known);
    }
    for (path, received) in observed {
        policy.observe_pair(path, received);
    }
}

async fn connect_bus(deadline: Instant) -> Option<Bus> {
    if Instant::now() >= deadline {
        return None;
    }
    match timeout_at(deadline, async {
        if !tokio::task::spawn_blocking(|| {
            Path::new("/sys/class/bluetooth/hci0").exists() && plugin_present()
        })
        .await
        .unwrap_or(false)
        {
            return Ok(None);
        }
        Bus::connect().await.map(Some)
    })
    .await
    {
        Ok(Ok(bus)) => bus,
        Ok(Err(e)) => {
            warn!("bluetooth: capability probe: {}", error_name(&e));
            None
        }
        Err(_) => {
            warn!("bluetooth: capability probe timed out");
            None
        }
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

enum PcmFallback {
    AwaitingAvrcp,
    AvrcpPlaying,
}

#[derive(Default)]
struct FrameActivity {
    // Порядок уже обслуженного запуска: AVRCP может пережить разрыв самого A2DP.
    start_floor: u64,
    active: Option<bool>,
    start: Option<u64>,
    // Метка первого PCM-буфера и порядок старта, которому он принадлежит.
    first: Option<(u64, Option<u64>)>,
    fallback: Option<PcmFallback>,
    covered_avr: Option<u64>,
    enable_continuation: Option<u64>,
    ignore_snapshot: bool,
}

impl FrameActivity {
    fn start(&mut self, order: u64) {
        self.first = Some((order, None));
        // Новое открытие PCM не означает новый старт уже идущего потока.
        if self.active != Some(true) {
            self.active = Some(true);
            self.start = Some(order);
        }
    }

    fn idle(&mut self) {
        self.active = Some(false);
        self.start = None;
        self.first = None;
        self.fallback = None;
        self.enable_continuation = None;
    }

    fn after_bus_loss(mut self) -> Self {
        self.ignore_snapshot = true;
        // Первый буфер после открытия не отличим от продолжения старого потока.
        self.start = None;
        self.first = None;
        self.fallback = None;
        self.enable_continuation = None;
        self.active = Some(true);
        self
    }

    fn worker_lost(&mut self, playing: bool) {
        self.first = None;
        self.start = None;
        if playing && self.active.is_none() {
            self.active = Some(true);
        }
    }

    fn first_frame_decision(
        &self,
        ready: Option<u64>,
        granted: Option<u64>,
        preempt_before: u64,
    ) -> Option<(u64, bool)> {
        let (token, start) = self.first?;
        if token >= preempt_before && start.is_some() && start == ready {
            (granted == ready).then_some((token, true))
        } else {
            Some((token, false))
        }
    }

    fn avr_order(&self, order: Option<u64>, snapshot_order: u64) -> Option<u64> {
        order
            .or((!self.ignore_snapshot).then_some(snapshot_order))
            .filter(|&order| Some(order) > self.covered_avr)
    }

    fn observe_snapshot(
        &mut self,
        policy: &mut Policy,
        avr: Option<(Playback, Option<u64>)>,
        snapshot_order: u64,
    ) {
        let avr = avr.map(|(phase, order)| (phase, self.avr_order(order, snapshot_order)));
        self.observe(policy, avr);
    }

    fn observe(&mut self, policy: &mut Policy, avr: Option<(Playback, Option<u64>)>) {
        if let Some(fallback) = &self.fallback {
            match avr {
                Some((Playback::Playing, order)) => {
                    // Первый поздний Playing подтверждает уже учтённые PCM-кадры.
                    self.covered_avr = self.covered_avr.max(order);
                    self.fallback = Some(PcmFallback::AvrcpPlaying);
                    return;
                }
                Some((Playback::Paused, _)) if matches!(fallback, PcmFallback::AvrcpPlaying) => {
                    // После подтверждённого Playing эта пауза отделяет следующий
                    // AVRCP-старт даже без простоя PCM. Исходный Paused не отделяет.
                    self.fallback = None;
                }
                _ => return,
            }
        }
        let first = self.start.take();
        let continuation = first.and_then(|_| self.enable_continuation.take());
        let (phase, order) = match (avr, first) {
            (Some((Playback::Playing, Some(order))), _)
                if first.is_none() || order > self.start_floor =>
            {
                // Первый PCM-буфер ещё относится к AVRCP-фронту. После простоя
                // тот же Playing уже не должен подменять новый фронт кадров.
                if self.first.is_some() {
                    self.covered_avr = self.covered_avr.max(Some(order));
                }
                (Some(Playback::Playing), Some(order))
            }
            (_, Some(order)) => {
                // Покрытый Playing не заменяет свежий фронт кадров после Idle.
                self.fallback = Some(if matches!(avr, Some((Playback::Playing, _))) {
                    PcmFallback::AvrcpPlaying
                } else {
                    PcmFallback::AwaitingAvrcp
                });
                (Some(Playback::Playing), Some(continuation.unwrap_or(order)))
            }
            (Some((phase, _)), None) => (Some(phase), None),
            (None, None) if self.active == Some(false) => (None, None),
            (None, None) => return,
        };
        policy.observe_playback(policy.generation, phase, order);
        if let Some(token) = first {
            self.first = Some((token, policy.admitted.then_some(policy.starts)));
        }
    }
}

struct PreviousEpisode {
    owner: String,
    starts: u64,
    frames: FrameActivity,
}

impl PreviousEpisode {
    fn recover(self, owner: &str, preempt_before: u64) -> Option<FrameActivity> {
        (self.owner == owner && self.starts < preempt_before).then(|| self.frames.after_bus_loss())
    }

    fn reopen_after_enable(
        &self,
        owner: &str,
        at: u64,
        preempt_before: u64,
    ) -> Option<FrameActivity> {
        (self.owner == owner && self.frames.fallback.is_some() && self.starts >= preempt_before)
            .then(|| FrameActivity {
                // Повторное открытие потока не даёт его первому буферу
                // вытеснить локальный источник, запущенный после On.
                enable_continuation: Some(at.max(self.starts)),
                covered_avr: self.frames.covered_avr,
                ..FrameActivity::default()
            })
    }
}

struct Service {
    shared: Rc<Shared>,
    model: &'static Model,
    stock: Stock,
    stock_ready: bool,
    events: UnboundedSender<BtEvent>,
    policy: Policy,
    seeded_pairs: bool,
    bus: Option<Bus>,
    probe: Option<PcmProbe>,
    reject_at: HashMap<String, Instant>,
    pcm_generation: Option<u64>,
    pcm_error: Option<&'static str>,
    flag_error: Option<&'static str>,
    visibility_target: Option<bool>,
    retry_at: Instant,
    enable_order: Option<u64>,
    frames: FrameActivity,
    previous_episode: Option<PreviousEpisode>,
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
                () = actors::shutdown_requested() => break,
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
                    match result {
                        Ok(pair) if flags.is_some() => {
                            if let Some((path, received)) = pair {
                                self.policy.observe_pair(path, received);
                            }
                            if flags != old && flags != Some([self.visibility_open(); 2]) {
                                self.retry_at = Instant::now();
                            }
                        }
                        _ => self.lost_bus().await,
                    }
                },
                ev = pcm_rx.recv() => match ev {
                    Some(ev) => self.on_pcm(ev),
                    None => break,
                },
            }
        }
        self.shared.pcm.signal_stop();
        if let Some(probe) = &mut self.probe {
            probe.wait_finished().await;
        }
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Enable { on, at } => {
                self.enable_order = at.filter(|_| !self.policy.enabled);
                if !on {
                    self.remember_episode();
                }
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

    fn remember_episode(&mut self) {
        if (self.policy.playback == Some(Playback::Playing) || self.frames.active == Some(true))
            && let Some(owner) = self.policy.owner.as_ref()
        {
            self.previous_episode = Some(PreviousEpisode {
                owner: owner.clone(),
                starts: self.policy.starts,
                frames: std::mem::take(&mut self.frames),
            });
        }
    }

    async fn lost_bus(&mut self) {
        let pinned = self.bus.take();
        let now = Instant::now();
        self.remember_episode();
        self.policy.release();
        self.policy.close_pairing(now);
        self.reset_session();
        self.flag_error = None;
        self.visibility_target = None;
        self.reject_at.clear();
        self.publish();
        self.close_offline_pairing(pinned).await;
        self.retry_at = Instant::now() + RETRY;
    }

    async fn close_offline_pairing(&mut self, pinned: Option<Bus>) {
        let pinned_closed = if let Some(bus) = pinned {
            matches!(
                timeout_at(Instant::now() + PROBE, bus.close_pairing()).await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        if !pinned_closed {
            match timeout_at(Instant::now() + PROBE, close_pairing()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    warn!("bluetooth: close pairing: {}", error_name(&e));
                    self.flag_error = Some("cannot close pairing flags");
                    return;
                }
                Err(_) => {
                    warn!("bluetooth: close pairing timed out");
                    self.flag_error = Some("cannot close pairing flags");
                    return;
                }
            }
        }
        self.visibility_target = Some(false);
        self.flag_error = None;
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
        self.shared.requested.get() && self.policy.deadline.is_some_and(|at| Instant::now() < at)
    }

    fn reconcile(&mut self) -> bool {
        let now = Instant::now();
        self.policy.expire(now);
        let enable_order = self.enable_order.take();
        if self.pcm_retry_at.is_some_and(|at| now >= at) {
            self.pcm_retry_at = None;
            self.pcm_error = None;
        }
        if let Some(bus) = &self.bus {
            self.policy.observe_pairs(|path| bus.is_paired(path));
            if !self.policy.enabled
                && self.previous_episode.as_ref().is_some_and(|episode| {
                    !bus.candidates().any(|c| c.device == episode.owner.as_str())
                })
            {
                self.previous_episode = None;
            }
            if let Some(owner) = self.policy.owner.as_deref()
                && (!self.policy.permits(owner) || selected(bus, &self.policy).is_none())
            {
                self.remember_episode();
                self.policy.release();
                self.reset_session();
            }
        }
        if self.stock_ready
            && let Some(bus) = &self.bus
        {
            let mut reenable = None;
            if self.shared.requested.get()
                && let Some(device) = self.policy.choose(
                    bus.candidates()
                        .filter(|c| !c.address.is_empty())
                        .map(|c| (c.device, c.sequence)),
                )
            {
                reenable = enable_order.filter(|_| {
                    self.previous_episode
                        .as_ref()
                        .is_some_and(|episode| episode.owner == device)
                });
                if let Some(frames) = self.previous_episode.take().and_then(|episode| {
                    let preempt_before = self.shared.preempt_before.get();
                    reenable
                        .and_then(|at| episode.reopen_after_enable(device, at, preempt_before))
                        .or_else(|| episode.recover(device, preempt_before))
                }) {
                    self.frames = frames;
                }
                self.frames.start_floor = self.policy.starts;
                self.policy.select(device.to_owned(), now);
            }
            if let Some(c) = selected(bus, &self.policy) {
                let last_start = self.policy.starts;
                self.frames
                    .observe_snapshot(&mut self.policy, c.phase, bus.snapshot_order());
                if let Some(enabled_at) = reenable
                    && let Some((Playback::Playing, order)) = c.phase
                    && let Some(observed) = self.frames.avr_order(order, bus.snapshot_order())
                    && observed <= last_start
                {
                    self.policy.reauthorize_playback(
                        observed,
                        enabled_at,
                        self.shared.preempt_before.get(),
                    );
                }
                if self.pcm_generation != Some(self.policy.generation)
                    && self.probe.as_ref().is_some_and(PcmProbe::ready)
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
        self.shared.pcm.reconcile_admission(
            self.policy.generation,
            ready.is_some() && self.granted == ready,
            self.frames
                .first_frame_decision(ready, self.granted, self.shared.preempt_before.get()),
        );
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
                self.frames
                    .worker_lost(self.policy.playback == Some(Playback::Playing));
                self.pcm_retry_at = Some(Instant::now() + RETRY);
            }
            PcmEvent::Started(g) if self.pcm_generation == Some(g) => self.pcm_ready = true,
            PcmEvent::Stopped(g) if self.pcm_generation == Some(g) => {
                self.pcm_ready = false;
                self.pcm_generation = None;
                self.policy.generation += 1;
                self.frames
                    .worker_lost(self.policy.playback == Some(Playback::Playing));
            }
            _ => {}
        }
    }

    async fn maintain(&mut self) {
        let now = Instant::now();
        let open = self.visibility_open();
        self.retry_at = now + RETRY;
        // BlueZ может перезапуститься, пока BlueALSA недоступен: старые
        // подтверждения не доказывают, что текущий адаптер закрыт для новых пар.
        if self.bus.is_none() {
            self.close_offline_pairing(None).await;
        }
        if !self.stock_ready {
            match self.stock.keep_stopped().await {
                Ok(()) => {
                    self.stock_ready = true;
                    self.retry_at = Instant::now();
                }
                Err(e) => warn!("bluetooth: cannot keep stock player stopped: {e}"),
            }
            return;
        }
        let Some(bus) = &self.bus else {
            if let Some(mut bus) = connect_bus(Instant::now() + PROBE).await {
                seed_startup_pairs(&mut bus, &mut self.policy, self.seeded_pairs);
                self.seeded_pairs = true;
                self.bus = Some(bus);
                self.visibility_target = None;
                self.retry_at = Instant::now();
            }
            return;
        };
        if self.probe.as_ref().is_some_and(PcmProbe::finished) {
            if self.pcm_generation.take().is_some() {
                self.shared.pcm.stop();
                self.pcm_ready = false;
                self.policy.generation += 1;
                self.frames
                    .worker_lost(self.policy.playback == Some(Playback::Playing));
            }
            self.probe = None;
        }
        if self.probe.is_none() {
            match self.shared.pcm.launch(self.model) {
                Ok(probe) => self.probe = Some(probe),
                Err(e) => warn!("bluetooth: cannot start PCM worker: {e}"),
            }
        }
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
            self.pcm_ready
                && self.probe.as_ref().is_some_and(PcmProbe::ready)
                && self.shared.requested.get(),
            self.shared.preempt_before.get(),
        )
    }

    fn publish(&self) {
        let c = self
            .bus
            .as_ref()
            .and_then(|bus| selected(bus, &self.policy));
        let pairing = self.stock_ready
            && self.visibility_open()
            && self.bus.as_ref().and_then(Bus::flags) == Some([true; 2]);
        let error = self
            .pcm_error
            .or(self.flag_error)
            .or_else(|| (!self.stock_ready).then_some("stock player not verified stopped"))
            .or_else(|| {
                self.bus
                    .is_none()
                    .then_some("Bluetooth service unavailable")
            })
            .or_else(|| {
                (!self.probe.as_ref().is_some_and(PcmProbe::ready))
                    .then_some("PCM output unavailable")
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
            let connection = next.connection;
            *old = next;
            let _ = self.events.send(BtEvent::Changed { connection });
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
        policy.select("phone".into(), Instant::now());
        policy
    }

    #[test]
    fn avr_start_keeps_its_order_while_waiting_for_pcm() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let phone_start = pulsar_core::audio_start_order();

        let music_start = pulsar_core::audio_start_order();
        assert!(phone_start < music_start);
        // Сигнал телефона уже принят, но актор ещё ожидал исходящий D-Bus вызов.
        frames.observe(&mut policy, Some((Playback::Playing, Some(phone_start))));
        assert_eq!(policy.ready_start(false, 0), None);
        assert_eq!(policy.ready_start(true, 0), Some(phone_start));
        assert_eq!(policy.ready_start(true, music_start), None);
        assert!(policy.preempt_before(music_start));
        frames.observe(&mut policy, Some((Playback::Playing, Some(phone_start))));
        assert!(!policy.preempt_before(music_start));
        assert_eq!(policy.ready_start(true, music_start), None);

        frames.observe(&mut policy, Some((Playback::Paused, None)));
        let next = pulsar_core::audio_start_order();
        frames.observe(&mut policy, Some((Playback::Playing, Some(next))));
        assert_eq!(policy.ready_start(true, music_start), Some(next));
    }

    #[test]
    fn new_playing_edge_with_unchanged_phase_can_reclaim_audio() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        frames.observe(&mut policy, Some((Playback::Playing, Some(10))));
        assert!(policy.preempt_before(20));
        frames.observe(&mut policy, Some((Playback::Playing, Some(10))));
        frames.observe(&mut policy, Some((Playback::Playing, Some(9))));
        assert_eq!(policy.ready_start(true, 20), None);
        frames.observe(&mut policy, Some((Playback::Playing, Some(30))));
        frames.observe(&mut policy, Some((Playback::Playing, Some(10))));
        assert_eq!(policy.ready_start(true, 20), Some(30));
    }

    #[test]
    fn resnapshot_and_first_reopened_frames_do_not_reclaim_displaced_audio() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        frames.observe_snapshot(&mut policy, Some((Playback::Playing, None)), 10);
        let previous = PreviousEpisode {
            owner: "phone".into(),
            starts: policy.starts,
            frames,
        };
        policy.release();
        // Музыка может начаться, пока шина Bluetooth недоступна.
        frames = previous.recover("phone", 20).unwrap();
        policy.select("phone".into(), Instant::now());
        frames.observe_snapshot(&mut policy, Some((Playback::Playing, None)), 30);
        assert_eq!(policy.playback, Some(Playback::Playing));
        assert_eq!(policy.ready_start(true, 20), None);
        frames.start(40);
        frames.observe_snapshot(&mut policy, None, 30);
        assert_eq!(policy.ready_start(true, 20), None);

        frames.observe_snapshot(&mut policy, Some((Playback::Playing, Some(50))), 30);
        assert_eq!(policy.ready_start(true, 20), Some(50));
    }

    #[test]
    fn pcm_prompt_keeps_its_edge_with_unchanged_paused_avrcp() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let paused = Some((Playback::Paused, None));
        frames.observe(&mut policy, paused);
        frames.start(10);
        assert_eq!(
            frames.first_frame_decision(None, None, 0),
            Some((10, false))
        );
        frames.observe(&mut policy, paused);
        frames.observe(&mut policy, paused);
        assert_eq!(policy.ready_start(true, 0), Some(10));
        assert_eq!(frames.first_frame_decision(Some(10), None, 0), None);
        assert_eq!(
            frames.first_frame_decision(Some(10), Some(10), 0),
            Some((10, true))
        );
        frames.observe(&mut policy, None);
        assert_eq!(policy.ready_start(true, 0), Some(10));
        assert_eq!(policy.ready_start(true, 20), None);
        frames.idle();
        frames.observe(&mut policy, paused);
        assert_eq!(policy.playback, Some(Playback::Paused));
        assert!(policy.ready_start(true, 0).is_none());
    }

    #[test]
    fn late_avrcp_playing_does_not_replay_preempted_pcm_prompt() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let paused = Some((Playback::Paused, None));
        frames.start(10);
        frames.observe(&mut policy, paused);
        assert!(policy.preempt_before(20));
        assert_eq!(
            frames.first_frame_decision(None, None, 20),
            Some((10, false))
        );
        let late_playing = Some((Playback::Playing, Some(30)));
        frames.observe_snapshot(&mut policy, late_playing, 1);
        assert_eq!(policy.ready_start(true, 20), None);

        // Первые кадры после переоткрытия не доказывают новый старт.
        frames.worker_lost(true);
        frames.start(35);
        frames.observe_snapshot(&mut policy, late_playing, 1);
        assert_eq!(policy.ready_start(true, 20), None);

        frames.idle();
        frames.observe_snapshot(&mut policy, late_playing, 1);
        assert_eq!(policy.ready_start(true, 20), None);
        assert_eq!(policy.starts, 10);

        // После подтверждённого простоя кадры доказывают новый старт,
        // даже если AVRCP всё ещё показывает тот же Playing.
        frames.start(40);
        frames.observe_snapshot(&mut policy, late_playing, 1);
        assert_eq!(policy.ready_start(true, 20), Some(40));
        assert_eq!(
            frames.first_frame_decision(Some(40), Some(40), 20),
            Some((40, true))
        );
    }

    #[test]
    fn fresh_avrcp_play_after_fallback_pause_needs_no_pcm_idle() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let paused = Some((Playback::Paused, None));
        frames.observe_snapshot(&mut policy, paused, 1);
        frames.start(10);
        frames.observe_snapshot(&mut policy, paused, 1);
        assert!(policy.preempt_before(20));

        let late_playing = Some((Playback::Playing, Some(30)));
        frames.observe_snapshot(&mut policy, late_playing, 1);
        frames.observe_snapshot(&mut policy, late_playing, 1);
        assert_eq!(policy.starts, 10);
        assert_eq!(policy.ready_start(true, 20), None);

        frames.observe_snapshot(&mut policy, paused, 1);
        assert_eq!(policy.playback, Some(Playback::Paused));
        assert_eq!(frames.active, Some(true));
        frames.observe_snapshot(&mut policy, Some((Playback::Playing, Some(40))), 1);
        assert_eq!(policy.ready_start(true, 20), Some(40));
    }

    #[test]
    fn newer_avrcp_grant_cannot_use_frames_captured_before_local_preemption() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        frames.start(10);
        frames.observe(&mut policy, Some((Playback::Playing, Some(30))));
        assert_eq!(policy.ready_start(true, 20), Some(30));
        assert_eq!(
            frames.first_frame_decision(Some(30), Some(30), 20),
            Some((10, false))
        );
    }

    #[test]
    fn reopening_paused_metadata_stream_keeps_the_explicit_enable_order() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let paused = Some((Playback::Paused, None));
        frames.start(10);
        frames.observe(&mut policy, paused);
        let previous = PreviousEpisode {
            owner: "phone".into(),
            starts: policy.starts,
            frames,
        };
        assert!(previous.reopen_after_enable("phone", 30, 20).is_none());
        let mut frames = previous.reopen_after_enable("phone", 30, 0).unwrap();
        policy.release();
        policy.select("phone".into(), Instant::now());
        frames.start(50);
        frames.observe(&mut policy, paused);
        assert_eq!(policy.ready_start(true, 0), Some(30));
        assert_eq!(policy.ready_start(true, 40), None);
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
        frames.worker_lost(true);
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
    fn profile_reconnect_accepts_pcm_even_if_avrcp_kept_its_old_playing_order() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let playing = Some((Playback::Playing, Some(10)));
        frames.observe_snapshot(&mut policy, playing, 1);
        frames.start(15);
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 0), Some(10));

        policy.release();
        policy.select("phone".into(), Instant::now());
        let mut frames = FrameActivity {
            start_floor: policy.starts,
            ..FrameActivity::default()
        };
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 0), None);
        frames.start(20);
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 0), Some(20));
        assert_eq!(
            frames.first_frame_decision(Some(20), Some(20), 0),
            Some((20, true))
        );
    }

    #[test]
    fn avrcp_first_worker_reopen_does_not_admit_pcm_until_a_confirmed_idle() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let playing = Some((Playback::Playing, Some(10)));
        frames.observe_snapshot(&mut policy, playing, 1);
        frames.observe_snapshot(&mut policy, playing, 1);
        frames.start(15);
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 0), Some(10));
        assert_eq!(
            frames.first_frame_decision(Some(10), Some(10), 0),
            Some((15, true))
        );
        assert!(policy.preempt_before(20));

        frames.worker_lost(true);
        frames.start(30);
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 20), None);
        assert_eq!(
            frames.first_frame_decision(None, None, 20),
            Some((30, false))
        );

        frames.idle();
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 20), None);
        frames.start(40);
        frames.observe_snapshot(&mut policy, playing, 1);
        assert_eq!(policy.ready_start(true, 20), Some(40));
        assert_eq!(
            frames.first_frame_decision(Some(40), Some(40), 20),
            Some((40, true))
        );
    }

    #[test]
    fn losing_avrcp_status_does_not_reuse_previously_observed_frames() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let phone_start = pulsar_core::audio_start_order();
        frames.observe(&mut policy, Some((Playback::Playing, Some(phone_start))));
        frames.start(pulsar_core::audio_start_order());
        frames.observe(&mut policy, Some((Playback::Playing, Some(phone_start))));
        frames.observe(&mut policy, Some((Playback::Paused, None)));
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
    fn losing_avrcp_status_over_idle_pcm_clears_playing_without_a_new_start() {
        let mut policy = selected_policy();
        let mut frames = FrameActivity::default();
        let phone_start = pulsar_core::audio_start_order();
        let avr = Some((Playback::Playing, Some(phone_start)));
        frames.observe(&mut policy, avr);
        frames.start(pulsar_core::audio_start_order());
        frames.observe(&mut policy, avr);
        frames.idle();
        frames.observe(&mut policy, avr);
        assert_eq!(policy.playback, Some(Playback::Playing));
        let music_start = pulsar_core::audio_start_order();
        assert!(policy.preempt_before(music_start));

        frames.observe(&mut policy, None);
        assert_eq!(policy.playback, None);
        assert_eq!(policy.starts, phone_start);
        assert_eq!(policy.ready_start(true, music_start), None);

        let restart = pulsar_core::audio_start_order();
        frames.start(restart);
        frames.observe(&mut policy, None);
        assert_eq!(policy.ready_start(true, music_start), Some(restart));
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
