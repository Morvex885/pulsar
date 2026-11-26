use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    Preempt(u64),
    Admit(u64),
}

struct Shared {
    state: RefCell<BtState>,
    requested: Cell<bool>,
    pcm: Worker,
}

#[derive(Clone)]
pub struct BtHandle {
    shared: Rc<Shared>,
    cmds: UnboundedSender<Cmd>,
}

impl BtHandle {
    pub fn state(&self) -> BtState {
        let mut state = self.shared.state.borrow().clone();
        state.admitted &= self.shared.requested.get();
        state
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
        let state = self.shared.state.borrow();
        if state.starts != 0 && state.starts < order {
            self.shared.pcm.set_admitted(false);
            let _ = self.cmds.send(Cmd::Preempt(state.starts));
        }
    }

    pub fn admit(&self, starts: u64) {
        let _ = self.cmds.send(Cmd::Admit(starts));
    }
}

pub async fn spawn(
    model: &'static Model,
    enabled: bool,
) -> Option<(BtHandle, UnboundedReceiver<BtEvent>)> {
    let stock = Stock::available()?;
    if !std::path::Path::new("/sys/class/bluetooth/hci0").exists() || !plugin_present() {
        return None;
    }
    let bus = match timeout(PROBE, Bus::connect()).await {
        Ok(Ok(bus)) => bus,
        Ok(Err(e)) => {
            warn!("bluetooth: capability probe: {}", error_name(&e));
            return None;
        }
        Err(_) => {
            warn!("bluetooth: capability probe timed out");
            return None;
        }
    };
    let (pcm, pcm_rx) = match Worker::spawn(model) {
        Ok(pair) => pair,
        Err(e) => {
            warn!("bluetooth: cannot start PCM worker: {e}");
            return None;
        }
    };
    pcm.set_station_gain(0.0);
    if let Err(e) = stock.take(bus.flags()?).await {
        warn!("bluetooth: cannot take stock player: {e}");
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
        pcm,
    });
    let svc = Service {
        shared: shared.clone(),
        events,
        policy,
        bus: Some(bus),
        reject_at: HashMap::new(),
        pcm_generation: None,
        pcm_error: None,
        flag_error: None,
        retry_at: Instant::now(),
        frame_start: None,
        granted: None,
        pcm_ready: false,
        pcm_retry_at: None,
    };
    actors::spawn_actor("bluetooth", svc.run(cmd_rx, pcm_rx));
    Some((BtHandle { shared, cmds }, ev_rx))
}

struct Service {
    shared: Rc<Shared>,
    events: UnboundedSender<BtEvent>,
    policy: Policy,
    bus: Option<Bus>,
    reject_at: HashMap<String, Instant>,
    pcm_generation: Option<u64>,
    pcm_error: Option<&'static str>,
    flag_error: Option<&'static str>,
    retry_at: Instant,
    frame_start: Option<u64>,
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
            self.reconcile();
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
                res = async { self.bus.as_mut().expect("guarded bus").next().await }, if self.bus.is_some() => {
                    if res.is_err() || self.bus.as_ref().is_some_and(|bus| bus.flags().is_none()) {
                        self.lost_bus();
                    }
                },
                ev = pcm_rx.recv() => match ev {
                    Some(ev) => self.on_pcm(ev),
                    None => {
                        self.pcm_error = Some("PCM worker stopped");
                        self.shared.pcm.stop();
                        self.policy.release();
                        self.publish();
                        break;
                    }
                },
            }
        }
        self.shared.pcm.stop();
    }

    async fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Enable(on) => {
                let paired = self.bus.as_ref().map_or_else(Default::default, Bus::paired);
                self.policy.set_enabled(on, Instant::now(), paired);
                if !on {
                    self.granted = None;
                    self.pcm_error = None;
                    self.pcm_retry_at = None;
                }
                self.retry_at = Instant::now();
            }
            Cmd::Preempt(starts) => {
                if self.policy.preempt(starts) {
                    self.granted = None;
                    self.shared.pcm.set_admitted(false);
                    self.publish();
                    self.avr(BtAction::Pause).await;
                }
            }
            Cmd::Admit(starts) => {
                if self.policy.starts == starts && self.policy.admitted {
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
        self.shared.pcm.stop();
        self.pcm_generation = None;
        self.pcm_error = None;
        self.pcm_retry_at = None;
        self.pcm_ready = false;
        self.granted = None;
        self.flag_error = None;
        self.reject_at.clear();
        self.retry_at = Instant::now() + RETRY;
    }

    fn reconcile(&mut self) {
        let had_window = self.policy.deadline.is_some();
        self.policy.expire(Instant::now());
        if self.pcm_retry_at.is_some_and(|at| Instant::now() >= at) {
            self.pcm_retry_at = None;
            self.pcm_error = None;
        }
        if let Some(bus) = &self.bus {
            self.policy.observe_pairs(&bus.paired());
            if let Some(owner) = self.policy.owner.as_deref()
                && (!self.policy.permits(owner) || !bus.candidates().any(|c| c.device == owner))
            {
                self.policy.release();
                self.shared.pcm.stop();
                self.pcm_generation = None;
                self.granted = None;
                self.pcm_ready = false;
                self.pcm_retry_at = None;
                self.pcm_error = None;
            }
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
                if self.pcm_generation != Some(self.policy.generation)
                    && self.pcm_error.is_none()
                    && self.shared.requested.get()
                {
                    self.frame_start = None;
                    self.pcm_ready = false;
                    self.shared
                        .pcm
                        .start(self.policy.generation, c.address.to_owned());
                    self.pcm_generation = Some(self.policy.generation);
                }
                self.shared.pcm.set_phone_gain(c.phone_gain);
                if self.pcm_ready {
                    // Кадры могли встать в очередь раньше фронта другого источника.
                    let (phase, order) = match c.phase {
                        Some(phase) => (Some(phase), None),
                        None => (
                            self.frame_start.map(|_| Playback::Playing),
                            self.frame_start,
                        ),
                    };
                    self.policy
                        .observe_playback(self.policy.generation, phase, order);
                }
            } else if self.pcm_generation.take().is_some() {
                self.pcm_ready = false;
                self.shared.pcm.stop();
            }
            if had_window != self.policy.deadline.is_some() {
                self.retry_at = Instant::now();
            }
        }
        if !self.policy.admitted {
            self.granted = None;
        }
        self.shared.pcm.set_admitted(
            self.pcm_ready
                && self.policy.admitted
                && self.granted == Some(self.policy.starts)
                && self.shared.requested.get(),
        );
        self.publish();
    }

    fn on_pcm(&mut self, ev: PcmEvent) {
        match ev {
            PcmEvent::Activity(g, order) if self.pcm_generation == Some(g) => {
                self.frame_start = Some(order);
            }
            PcmEvent::Idle(g) if self.pcm_generation == Some(g) => self.frame_start = None,
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
            }
            _ => {}
        }
    }

    async fn maintain(&mut self) {
        let now = Instant::now();
        self.retry_at = now + RETRY;
        let Some(bus) = &self.bus else {
            if let Ok(Ok(bus)) = timeout(PROBE, Bus::connect()).await {
                self.bus = Some(bus);
                self.retry_at = Instant::now();
            }
            return;
        };
        let open = self.policy.deadline.is_some() && self.shared.requested.get();
        if bus.flags() != Some([open; 2]) {
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
        self.reject_at
            .retain(|device, _| bus.candidates().any(|c| c.device == device));
        // Не задерживать обработку новых сигналов за серией D-Bus вызовов к нескольким телефонам.
        if let Some(c) = bus.candidates().find(|c| {
            self.policy.owner.as_deref() != Some(c.device)
                && self.policy.deadline.is_none()
                && self.reject_at.get(c.device).is_none_or(|&at| now >= at)
        }) {
            self.reject_at.insert(c.device.to_owned(), now + RETRY);
            if let Err(e) = bus.disconnect(c.device).await {
                warn!("bluetooth: DisconnectProfile: {}", error_name(&e));
            }
        }
        self.publish();
    }

    fn publish(&self) {
        let c = self
            .bus
            .as_ref()
            .and_then(|bus| selected(bus, &self.policy));
        let pairing = self.policy.deadline.is_some()
            && self.bus.as_ref().and_then(Bus::flags) == Some([true; 2]);
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
        let next = BtState {
            enabled: self.policy.enabled,
            pairing,
            status,
            playback: self.policy.playback,
            admitted: self.pcm_ready && self.policy.admitted && self.shared.requested.get(),
            starts: self.policy.starts,
            connection: c.and(self.policy.connection),
        };
        let mut old = self.shared.state.borrow_mut();
        if *old != next {
            *old = next;
            let _ = self.events.send(BtEvent::Changed);
        }
    }
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
