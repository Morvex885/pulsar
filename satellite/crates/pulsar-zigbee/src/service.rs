//! The Midi's Zigbee radio in one of its roles:
//!   off:         pulsar leaves the radio alone (whatever firmware it has keeps running); the
//!                default, so an install never writes the radio on its own;
//!   coordinator: pulsar's coordinator firmware (radio/coordinator, installed next to the daemon),
//!                a deCONZ coordinator for ZHA and Zigbee2MQTT (Coordinator + DeconzServer);
//!   router:      pulsar's router firmware (radio/router), a repeater in another network (Router).
//! At start the running firmware is identified (0x0900) before the role object is built. When it
//! is not the role's image (another role, another build: a pulsar update brought a new image), or
//! the radio is silent (an interrupted upload), the role's image is written through the radio's
//! bootloader (flasher). At most two uploads per start; an image the radio did not take is
//! reported (`FlashOutcome`, persisted by the controller as `failed_version`) and not written
//! again on later starts. Each firmware keeps its own network in its own flash area, so
//! switching back and forth keeps them.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use log::{error, info, warn};
use pulsar_core::{Timers, actors};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::bytes::get_be32;
use crate::coordinator::{ApsFrame, Coordinator, Hooks};
use crate::deconz::DeconzServer;
use crate::flasher::{radio_flash, ybl_load};
use crate::link::{Link, LinkEvent};
use crate::router::Router;
use crate::{Mode, RadioLines, role};

/// Reset + boot + 0x0900 normally take < 1 s.
const ANSWER_TIMEOUT_MS: u64 = 10_000;
/// A silent radio is asked again.
const RETRY_MS: u64 = 60_000;
/// Per start: the image, and one retry.
const MAX_FLASHES: u32 = 2;
const APP_INFO_REQ: u16 = 0x0900;
const APP_INFO_RSP: u16 = 0x8900;
/// Yandex's gateway.ybl.
const GATEWAY_FAMILY: u8 = 0x15;

pub struct ZigbeeConfig {
    pub lines: RadioLines,
    /// zigbee-coordinator.ybl
    pub coordinator_image: PathBuf,
    /// zigbee-router.ybl
    pub router_image: PathBuf,
    pub deconz_port: u16,
    /// Image version given up on in an earlier start (0: none).
    pub failed_version: u32,
}

/// C++ `changed()` and `outcome(failed)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZigbeeEvent {
    /// `status()` / `flashing()` may have changed.
    Changed,
    /// The role runs (0), or the radio did not take image version `failed_version`: persist it.
    FlashOutcome { failed_version: u32 },
}

enum Cmd {
    Join,
    Leave,
}

struct Shared {
    status: RefCell<String>,
    flashing: Cell<bool>,
}

#[derive(Clone)]
pub struct ZigbeeHandle {
    mode: Mode,
    shared: Rc<Shared>,
    cmds: UnboundedSender<Cmd>,
}

impl ZigbeeHandle {
    /// Constant: a role change restarts the daemon.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// An image is being written (the actor starts it on the radio's answer or its timeout).
    pub fn flashing(&self) -> bool {
        self.shared.flashing.get()
    }

    /// One line for Home Assistant, e.g. "router: in network 0x1A2B as 0x0001, channel 15".
    pub fn status(&self) -> String {
        self.shared.status.borrow().clone()
    }

    /// Router role only: join the network that currently allows joining. The status changes
    /// once the actor ran the command (then `Changed`), not inside this call as in C++.
    pub fn join(&self) {
        let _ = self.cmds.send(Cmd::Join);
    }

    /// Router role only: leave the current network (no `Changed`, as in C++).
    pub fn leave(&self) {
        let _ = self.cmds.send(Cmd::Leave);
    }
}

/// How the actor reaches the radio; tests replace both.
pub(crate) struct Backend {
    pub open: OpenFn,
    pub flash: FlashFn,
}

pub(crate) type OpenFn = Box<dyn FnMut(&RadioLines) -> Option<Link>>;
pub(crate) type FlashFn =
    Arc<dyn Fn(&RadioLines, &[u8], &mut dyn FnMut(f32)) -> Result<(), String> + Send + Sync>;

/// Starts the role. The initial status ("off", "starting" or "error: cannot open the radio") is
/// there when this returns, with its `Changed` already queued.
pub fn spawn(cfg: ZigbeeConfig, mode: Mode) -> (ZigbeeHandle, UnboundedReceiver<ZigbeeEvent>) {
    spawn_with(
        cfg,
        mode,
        Backend {
            open: Box::new(Link::open),
            flash: Arc::new(radio_flash),
        },
    )
}

pub(crate) fn spawn_with(
    cfg: ZigbeeConfig,
    mode: Mode,
    backend: Backend,
) -> (ZigbeeHandle, UnboundedReceiver<ZigbeeEvent>) {
    let (ev_tx, ev_rx) = unbounded_channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (flash_tx, flash_rx) = unbounded_channel();
    let shared = Rc::new(Shared {
        status: RefCell::new(String::new()),
        flashing: Cell::new(false),
    });
    let mut svc = Service {
        cfg,
        mode,
        backend,
        shared: shared.clone(),
        events: ev_tx,
        phase: Phase::Off,
        error: String::new(),
        progress: 0.0,
        flashes: 0,
        timers: Timers::new(),
        flasher: None,
        flash_tx,
        link: None,
        coord: None,
        deconz: None,
        router: None,
    };
    svc.start();
    actors::spawn_actor("zigbee", svc.run_actor(cmd_rx, flash_rx));
    (
        ZigbeeHandle {
            mode,
            shared,
            cmds: cmd_tx,
        },
        ev_rx,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Off,
    Starting,
    Running,
    Flashing,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    /// The radio must identify its firmware in time.
    Answer,
    /// A silent radio is asked again.
    Retry,
}

enum FlashMsg {
    Progress(f32),
    Done(Result<(), String>),
}

enum Wake {
    Timer(Timer),
    Link(LinkEvent),
    Coord(crate::coordinator::Wake),
    Deconz(crate::deconz::Wake),
    Router(crate::router::Wake),
    Flash(FlashMsg),
    Cmd(Option<Cmd>),
}

struct Service {
    cfg: ZigbeeConfig,
    mode: Mode,
    backend: Backend,
    shared: Rc<Shared>,
    events: UnboundedSender<ZigbeeEvent>,
    phase: Phase,
    error: String,
    progress: f32,
    /// Images written since start.
    flashes: u32,
    timers: Timers<Timer>,
    flasher: Option<std::thread::JoinHandle<()>>,
    flash_tx: UnboundedSender<FlashMsg>,
    link: Option<Link>,
    coord: Option<Coordinator>,
    deconz: Option<DeconzServer>,
    router: Option<Router>,
}

/// The coordinator's hooks: the deCONZ server's, plus the service's `changed()`.
struct CoordHooks<'a> {
    deconz: &'a mut DeconzServer,
    changed: bool,
}

impl Hooks for CoordHooks<'_> {
    fn received(&mut self, coord: &mut Coordinator, f: &ApsFrame) {
        self.deconz.received(coord, f);
    }

    fn sent(&mut self, coord: &mut Coordinator, handle: u32, status: u8) {
        self.deconz.sent(coord, handle, status);
    }

    fn changed(&mut self, coord: &mut Coordinator) {
        self.deconz.changed(coord);
        self.changed = true;
    }

    fn formed(&mut self, coord: &mut Coordinator, ok: bool) {
        self.deconz.formed(coord, ok);
    }
}

/// What to do with the firmware the radio reported (C++ `on_firmware`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The role's image runs: `FlashOutcome { 0 }`, then the role.
    Run,
    /// Same family, but the role's image was not taken earlier: keep what runs.
    Keep,
    /// Write the role's image.
    Flash,
}

/// `want`: the role's image version, None without a readable image (then only the family
/// counts).
pub(crate) fn decide(version: u32, want: Option<u32>, mode: Mode, failed_version: u32) -> Decision {
    // The role's image decides, as maind does it for the gateway: another version of the same
    // firmware is replaced too (a pulsar update brings a new build).
    let same_family = (version >> 24) as u8 == role(mode).family;
    let matches = match want {
        Some(w) => version == w,
        None => same_family,
    };
    if matches {
        Decision::Run
    } else if same_family && want.unwrap_or(0) == failed_version {
        Decision::Keep
    } else {
        Decision::Flash
    }
}

fn family_name(version: u32) -> &'static str {
    match (version >> 24) as u8 {
        GATEWAY_FAMILY => "Yandex's gateway",
        f if f == role(Mode::Router).family => "router",
        f if f == role(Mode::Coordinator).family => "coordinator",
        _ => "unknown",
    }
}

impl Service {
    fn image_path(&self) -> &PathBuf {
        if self.mode == Mode::Router {
            &self.cfg.router_image
        } else {
            &self.cfg.coordinator_image
        }
    }

    /// C++ `changed()`: publish the state, then tell the controller.
    fn changed(&mut self) {
        self.publish();
        let _ = self.events.send(ZigbeeEvent::Changed);
    }

    fn outcome(&mut self, failed_version: u32) {
        let _ = self
            .events
            .send(ZigbeeEvent::FlashOutcome { failed_version });
    }

    /// The getters' state, as C++ `status()` / `flashing()` would answer now.
    fn publish(&self) {
        let status = self.status();
        if *self.shared.status.borrow() != status {
            *self.shared.status.borrow_mut() = status;
        }
        self.shared.flashing.set(self.phase == Phase::Flashing);
    }

    fn start(&mut self) {
        info!("zigbee: role {}", role(self.mode).name);
        if self.mode == Mode::Off {
            return self.changed();
        }
        self.bring_up();
    }

    /// Opens the radio and asks which firmware runs; the role object comes once it is the
    /// right one.
    fn bring_up(&mut self) {
        self.phase = Phase::Starting;
        let Some(link) = (self.backend.open)(&self.cfg.lines) else {
            return self.fail("cannot open the radio".into());
        };
        self.link = Some(link);
        self.timers.set(Timer::Answer, ANSWER_TIMEOUT_MS);
        self.changed();
    }

    fn tear_down(&mut self) {
        self.deconz = None; // works on the coordinator
        self.coord = None;
        self.router = None;
        self.link = None;
    }

    fn on_starting_link(&mut self, ev: LinkEvent) {
        match ev {
            LinkEvent::Boot => {
                if let Some(l) = &mut self.link {
                    l.send(APP_INFO_REQ, &[], None);
                }
            }
            // [version BE32][manufacturer BE16][image type BE16][size BE32], the same from
            // every firmware
            LinkEvent::Frame { ty, payload } if ty == APP_INFO_RSP && payload.len() >= 12 => {
                self.on_firmware(get_be32(&payload));
            }
            _ => {}
        }
    }

    fn on_firmware(&mut self, version: u32) {
        if self.phase != Phase::Starting {
            return;
        }
        self.timers.cancel(Timer::Answer);
        let path = self.image_path().clone();
        let want = ybl_load(&path).ok().map(|(_, info)| info.version);
        info!("zigbee: radio firmware 0x{version:08x}");
        match decide(version, want, self.mode, self.cfg.failed_version) {
            Decision::Run => {
                self.outcome(0);
                self.run();
            }
            Decision::Keep => {
                // The radio did not take the new build.
                warn!(
                    "zigbee: keeping radio firmware 0x{version:08x} (0x{:08x} was not taken)",
                    want.unwrap_or(0)
                );
                self.run();
            }
            Decision::Flash => {
                info!(
                    "zigbee: radio runs {} firmware, the {} role needs {} 0x{:08x}",
                    family_name(version),
                    role(self.mode).name,
                    path.display(),
                    want.unwrap_or(0)
                );
                self.flash();
            }
        }
    }

    fn run(&mut self) {
        if self.phase != Phase::Starting {
            return;
        }
        let Some(link) = self.link.take() else {
            return;
        };
        self.phase = Phase::Running;
        self.error.clear();
        if self.mode == Mode::Router {
            let mut router = Router::new(link);
            router.start();
            self.router = Some(router);
            return self.changed();
        }
        let mut coord = Coordinator::new(link);
        let mut deconz = DeconzServer::new(self.cfg.deconz_port);
        deconz.attach(&mut coord);
        if !deconz.start() {
            self.coord = Some(coord);
            return self.fail("cannot open the deCONZ port".into());
        }
        let mut hooks = CoordHooks {
            deconz: &mut deconz,
            changed: false,
        };
        coord.start(&mut hooks);
        self.coord = Some(coord);
        self.deconz = Some(deconz);
        // One Changed, whether or not the start already reported one.
        self.changed();
    }

    fn flash(&mut self) {
        if self.phase == Phase::Flashing {
            return;
        }
        self.tear_down();
        let path = self.image_path().clone();
        let (image, info) = match ybl_load(&path) {
            Ok(v) => v,
            Err(e) => return self.fail(e),
        };
        if info.version == self.cfg.failed_version {
            // Given up on in an earlier start: not written on every restart. A new image
            // (update) or a role change tries again.
            return self.fail(format!(
                "the radio did not take firmware 0x{:08x}; not written again",
                info.version
            ));
        }
        if self.flashes >= MAX_FLASHES {
            self.outcome(info.version);
            return self.fail(format!(
                "the radio does not take firmware 0x{:08x}",
                info.version
            ));
        }
        self.flashes += 1;
        self.phase = Phase::Flashing;
        self.progress = 0.0;
        info!(
            "zigbee: writing {} (version 0x{:08x}, {} bytes) to the radio",
            path.display(),
            info.version,
            image.len()
        );
        self.changed();
        let lines = self.cfg.lines.clone();
        let flash = self.backend.flash.clone();
        let tx = self.flash_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("zigbee-flash".into())
            .spawn(move || {
                let mut last = -1i32;
                let r = flash(&lines, &image, &mut |p| {
                    // Steps of 5 %: a status change per step is enough for Home Assistant.
                    let pct = (p * 100.0) as i32;
                    if pct / 5 == last / 5 {
                        return;
                    }
                    last = pct;
                    let _ = tx.send(FlashMsg::Progress(p));
                });
                let _ = tx.send(FlashMsg::Done(r));
            });
        match spawned {
            Ok(t) => self.flasher = Some(t),
            Err(e) => {
                let _ = self
                    .flash_tx
                    .send(FlashMsg::Done(Err(format!("thread: {e}"))));
            }
        }
    }

    fn on_flash(&mut self, m: FlashMsg) {
        match m {
            FlashMsg::Progress(p) => {
                self.progress = p;
                self.changed();
            }
            FlashMsg::Done(r) => {
                if let Some(t) = self.flasher.take() {
                    let _ = t.join();
                }
                match r {
                    Ok(()) => info!("zigbee: radio firmware written"),
                    Err(e) => {
                        error!("zigbee: writing the radio failed: {e}");
                        self.error = e;
                    }
                }
                // Either way the radio runs some application again (or none): identify it.
                self.bring_up();
            }
        }
    }

    fn fail(&mut self, why: String) {
        error!("zigbee: {why}");
        self.tear_down();
        self.phase = Phase::Failed;
        self.error = why;
        self.changed();
    }

    fn on_timer(&mut self, t: Timer) {
        match t {
            Timer::Answer => {
                if self.phase != Phase::Starting {
                    return;
                }
                warn!("zigbee: the radio does not identify its firmware");
                // Most likely an interrupted upload left no application, which only writing the
                // image fixes.
                self.flash();
                if self.phase == Phase::Failed {
                    // Writing refused: maybe it answers later.
                    self.timers.set(Timer::Retry, RETRY_MS);
                }
            }
            Timer::Retry => self.bring_up(),
        }
    }

    fn on_cmd(&mut self, c: Cmd) {
        if self.phase != Phase::Running {
            return;
        }
        let Some(router) = &mut self.router else {
            return;
        };
        match c {
            Cmd::Join => {
                if router.join() {
                    self.changed();
                }
            }
            Cmd::Leave => router.leave(),
        }
    }

    fn status(&self) -> String {
        match self.phase {
            Phase::Off => return "off".into(),
            Phase::Flashing => {
                return format!(
                    "writing {} firmware: {} %",
                    role(self.mode).label,
                    (self.progress * 100.0) as i32
                );
            }
            Phase::Failed => return format!("error: {}", self.error),
            Phase::Starting => return "starting".into(),
            Phase::Running => {}
        }
        if self.mode == Mode::Coordinator {
            let Some(c) = self.coord.as_ref().filter(|c| c.ready()) else {
                return "coordinator: radio not ready".into();
            };
            let n = c.network();
            if n.forming {
                return "coordinator: forming a network".into();
            }
            if !n.formed {
                return "coordinator: no network".into();
            }
            return format!("coordinator: PAN 0x{:04X}, channel {}", n.pan, n.channel);
        }
        let s = self
            .router
            .as_ref()
            .map(|r| r.state().clone())
            .unwrap_or_default();
        if !s.ready {
            return "router: radio not ready".into();
        }
        if s.joining {
            return "router: joining".into();
        }
        if s.joined {
            return format!(
                "router: in network 0x{:04X} as 0x{:04X}, channel {}",
                s.pan, s.nwk, s.channel
            );
        }
        if s.error.is_empty() {
            "router: not in a network".into()
        } else {
            format!("router: {}", s.error)
        }
    }

    async fn next(
        &mut self,
        cmds: &mut UnboundedReceiver<Cmd>,
        flash_rx: &mut UnboundedReceiver<FlashMsg>,
    ) -> Wake {
        let Service {
            timers,
            link,
            coord,
            deconz,
            router,
            ..
        } = self;
        tokio::select! {
            biased;
            t = timers.next() => Wake::Timer(t),
            ev = async { link.as_mut().unwrap().next().await }, if link.is_some() => Wake::Link(ev),
            w = async { coord.as_mut().unwrap().next().await }, if coord.is_some() => Wake::Coord(w),
            w = async { deconz.as_mut().unwrap().next().await }, if deconz.is_some() => {
                Wake::Deconz(w)
            }
            w = async { router.as_mut().unwrap().next().await }, if router.is_some() => {
                Wake::Router(w)
            }
            Some(m) = flash_rx.recv() => Wake::Flash(m),
            c = cmds.recv() => Wake::Cmd(c),
        }
    }

    fn handle(&mut self, w: Wake) {
        // Firmware identification and the deCONZ host do not change the status by themselves
        // (what they change goes through changed()).
        let republish = !matches!(w, Wake::Link(_) | Wake::Deconz(_));
        match w {
            Wake::Timer(t) => self.on_timer(t),
            Wake::Link(ev) => self.on_starting_link(ev),
            Wake::Coord(w) => {
                let (Some(coord), Some(deconz)) = (&mut self.coord, &mut self.deconz) else {
                    return;
                };
                let mut hooks = CoordHooks {
                    deconz,
                    changed: false,
                };
                coord.handle(w, &mut hooks);
                if hooks.changed {
                    self.changed();
                }
            }
            Wake::Deconz(w) => {
                if let (Some(coord), Some(deconz)) = (&mut self.coord, &mut self.deconz) {
                    deconz.handle(w, coord);
                }
            }
            Wake::Router(w) => {
                if self.router.as_mut().is_some_and(|r| r.handle(w)) {
                    self.changed();
                }
            }
            Wake::Flash(m) => self.on_flash(m),
            Wake::Cmd(Some(c)) => self.on_cmd(c),
            Wake::Cmd(None) => {}
        }
        // The getters also follow changes C++ reported without changed() (a leave, the
        // router's state on a frame that is not news); no Changed for them, as in C++.
        if republish {
            self.publish();
        }
    }

    async fn run_actor(
        mut self,
        mut cmds: UnboundedReceiver<Cmd>,
        mut flash_rx: UnboundedReceiver<FlashMsg>,
    ) {
        loop {
            match self.next(&mut cmds, &mut flash_rx).await {
                Wake::Cmd(None) => break,
                w @ Wake::Deconz(_) => {
                    self.handle(w);
                    // A flooding LAN client must not starve the other actors.
                    tokio::task::coop::consume_budget().await;
                }
                w => self.handle(w),
            }
        }
        // radio_flash cannot be stopped: an upload still running is left to its detached thread
        // (dropping the JoinHandle), which the process exit ends. That leaves the radio without
        // an application; the next start finds it silent and writes the image again (C++ joined
        // it in the destructor, and the 5 s teardown watchdog cut it the same way).
        self.flasher = None;
        self.tear_down();
    }
}

#[cfg(test)]
mod tests;
