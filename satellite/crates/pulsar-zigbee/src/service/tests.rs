use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::task::LocalSet;

use super::*;
use crate::bytes::put_le16;
use crate::test_util::{Lines, Radio, absent_lines, link_pair, ybl_image};

const ROUTER_IMAGE: u32 = 0x5001_0203;
const COORD_IMAGE: u32 = 0x5101_0203;
const GATEWAY: u32 = 0x1500_0351;

#[test]
fn firmware_decisions() {
    use Decision::*;
    let r = Mode::Router;
    // The role's image runs.
    assert_eq!(decide(ROUTER_IMAGE, Some(ROUTER_IMAGE), r, 0), Run);
    // Another build of the same firmware is replaced...
    assert_eq!(decide(0x5000_0001, Some(ROUTER_IMAGE), r, 0), Flash);
    // ...unless the radio did not take this image before: keep what runs.
    assert_eq!(
        decide(0x5000_0001, Some(ROUTER_IMAGE), r, ROUTER_IMAGE),
        Keep
    );
    // Another family is replaced, whatever failed earlier.
    assert_eq!(decide(GATEWAY, Some(ROUTER_IMAGE), r, 0), Flash);
    assert_eq!(
        decide(COORD_IMAGE, Some(ROUTER_IMAGE), r, ROUTER_IMAGE),
        Flash
    );
    assert_eq!(
        decide(ROUTER_IMAGE, Some(COORD_IMAGE), Mode::Coordinator, 0),
        Flash
    );
    // Without a readable image only the family counts.
    assert_eq!(decide(0x5000_0001, None, r, 0), Run);
    assert_eq!(decide(GATEWAY, None, r, 0), Flash);
    assert_eq!(decide(0x51FF_FFFF, None, Mode::Coordinator, 0), Run);
    assert_eq!(family_name(GATEWAY), "Yandex's gateway");
    assert_eq!(family_name(ROUTER_IMAGE), "router");
    assert_eq!(family_name(COORD_IMAGE), "coordinator");
    assert_eq!(family_name(0x1234_5678), "unknown");
}

/// What the emulated radio runs, per opening of the link.
#[derive(Clone)]
enum Fw {
    /// Answers 0x0900 with this version; as a router or coordinator it also answers its
    /// commands.
    Runs(u32),
    /// No ACK, no answer (an interrupted upload).
    Silent,
}

struct Rig {
    radios: Rc<RefCell<Vec<(Radio, Fw)>>>,
    fws: Rc<RefCell<VecDeque<Fw>>>,
    flashed: Arc<Mutex<u32>>,
    /// The fake flasher finishes only once this is set.
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    /// The router firmware reports itself on a network.
    joined: Rc<Cell<bool>>,
    /// Status when spawn returned; its Changed is taken off the channel.
    initial: String,
    dir: Lines,
    rx: UnboundedReceiver<ZigbeeEvent>,
    handle: ZigbeeHandle,
    seen: Vec<String>,
}

fn router_info(on: bool) -> Vec<u8> {
    let mut v = vec![0x02, 0x8E, u8::from(on), 0x1A, 0x62];
    v.extend([0xA0; 8]);
    v.extend_from_slice(&[0x4F, 0x21]);
    v.extend([0xB0; 8]);
    v.push(15);
    v
}

fn coord_state() -> Vec<u8> {
    let mut v = vec![2, 0, 15];
    put_le16(&mut v, 0x1A62);
    v.extend_from_slice(&[0; 40]);
    v
}

impl Rig {
    /// `fws`: what the radio runs at each opening (the last one repeats); `open_fails`: no tty.
    fn spawn(mode: Mode, fws: &[Fw], failed_version: u32, open_fails: bool) -> Rig {
        let dir = Lines::new();
        let router_image = dir.dir().join("zigbee-router.ybl");
        let coordinator_image = dir.dir().join("zigbee-coordinator.ybl");
        std::fs::write(&router_image, ybl_image(ROUTER_IMAGE)).unwrap();
        std::fs::write(&coordinator_image, ybl_image(COORD_IMAGE)).unwrap();
        let radios: Rc<RefCell<Vec<(Radio, Fw)>>> = Rc::default();
        let fw_queue: Rc<RefCell<VecDeque<Fw>>> =
            Rc::new(RefCell::new(fws.iter().cloned().collect()));
        let flashed = Arc::new(Mutex::new(0));
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let open = {
            let radios = radios.clone();
            let fw_queue = fw_queue.clone();
            Box::new(move |_: &RadioLines| {
                if open_fails {
                    return None;
                }
                let (link, radio) = link_pair(&absent_lines());
                let mut q = fw_queue.borrow_mut();
                let fw = if q.len() > 1 {
                    q.pop_front()
                } else {
                    q.front().cloned()
                };
                radios.borrow_mut().push((radio, fw.unwrap()));
                Some(link)
            })
        };
        let flash: FlashFn = {
            let flashed = flashed.clone();
            let gate = gate.clone();
            Arc::new(move |_, image, progress| {
                assert!(crate::flasher::ybl_parse(image).is_ok());
                let (open, cv) = &*gate;
                let mut open = open.lock().unwrap();
                while !*open {
                    open = cv.wait(open).unwrap();
                }
                drop(open);
                for i in 0..=100 {
                    progress(i as f32 / 100.0);
                }
                *flashed.lock().unwrap() += 1;
                Ok(())
            })
        };
        let cfg = ZigbeeConfig {
            lines: absent_lines(),
            coordinator_image,
            router_image,
            deconz_port: 0,
            failed_version,
        };
        let (handle, mut rx) = spawn_with(cfg, mode, Backend { open, flash });
        let initial = handle.status();
        assert_eq!(rx.try_recv(), Ok(ZigbeeEvent::Changed));
        Rig {
            gate,
            joined: Rc::new(Cell::new(false)),
            initial,
            radios,
            fws: fw_queue,
            flashed,
            dir,
            rx,
            handle,
            seen: Vec::new(),
        }
    }

    /// Lets the actor and the radio run (no time passes).
    async fn pump(&mut self) {
        for _ in 0..200 {
            tokio::task::yield_now().await;
            let joined = self.joined.get();
            let mut radios = self.radios.borrow_mut();
            let Some((radio, fw)) = radios.last_mut() else {
                continue;
            };
            while let Some((ty, _)) = radio.read_frame() {
                let Fw::Runs(version) = *fw else {
                    continue;
                };
                radio.ack(ty, 0);
                match ty {
                    0x0900 => {
                        let mut p = version.to_be_bytes().to_vec();
                        p.extend_from_slice(&[0x14, 0x0A, 0x05, 0x80, 0, 0, 2, 0]);
                        radio.send(0x8900, &p);
                    }
                    0x0045 => radio.send(0x8045, &router_info(joined)),
                    0x0A00 => radio.send(0x8A00, &coord_state()),
                    _ => {}
                }
            }
        }
        while let Ok(ev) = self.rx.try_recv() {
            self.seen.push(match ev {
                ZigbeeEvent::Changed => format!("changed {}", self.handle.status()),
                ZigbeeEvent::FlashOutcome { failed_version } => {
                    format!("outcome 0x{failed_version:08x}")
                }
            });
        }
    }

    /// Lets the flasher finish, waits for its thread, then pumps.
    async fn after_flash(&mut self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(5));
            self.pump().await;
            if !self.handle.flashing() {
                return;
            }
        }
        panic!("flash did not finish");
    }

    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.seen)
    }

    fn opens(&self) -> usize {
        self.radios.borrow().len()
    }
}

async fn local<F: Future<Output = ()>>(f: F) {
    LocalSet::new().run_until(f).await;
}

#[tokio::test(start_paused = true)]
async fn off_does_nothing() {
    local(async {
        let mut r = Rig::spawn(Mode::Off, &[Fw::Runs(GATEWAY)], 0, false);
        // The first Changed is queued before spawn returns.
        assert_eq!(r.initial, "off");
        r.handle.join();
        r.pump().await;
        assert_eq!(r.opens(), 0);
        assert_eq!(r.handle.mode(), Mode::Off);
        assert!(r.take().is_empty());
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn radio_that_cannot_be_opened() {
    local(async {
        let r = Rig::spawn(Mode::Router, &[Fw::Silent], 0, true);
        assert_eq!(r.initial, "error: cannot open the radio");
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn router_runs_joins_and_leaves() {
    local(async {
        let mut r = Rig::spawn(Mode::Router, &[Fw::Runs(ROUTER_IMAGE)], 0, false);
        assert_eq!(r.initial, "starting");
        r.pump().await;
        assert_eq!(
            r.take(),
            [
                "outcome 0x00000000",
                "changed router: not in a network",
                "changed router: not in a network"
            ]
        );
        // join: Changed once the actor ran it; leave: no Changed, the status follows.
        r.handle.join();
        r.pump().await;
        assert_eq!(r.take(), ["changed router: joining"]);
        r.handle.leave();
        r.pump().await;
        assert!(r.take().is_empty());
        assert_eq!(r.handle.status(), "router: not in a network");
        // Joined (0x8906, then the channel from 0x8045).
        r.joined.set(true);
        {
            let mut radios = r.radios.borrow_mut();
            let radio = &mut radios.last_mut().unwrap().0;
            let mut joined = router_info(true);
            joined.pop();
            radio.send(0x8906, &joined);
            radio.send(0x8045, &router_info(true));
        }
        r.pump().await;
        assert_eq!(
            r.handle.status(),
            "router: in network 0x1A62 as 0x4F21, channel 15"
        );
        assert!(!r.handle.flashing());
        assert_eq!(*r.flashed.lock().unwrap(), 0);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn other_firmware_is_written_then_identified() {
    local(async {
        let mut r = Rig::spawn(
            Mode::Router,
            &[Fw::Runs(GATEWAY), Fw::Runs(ROUTER_IMAGE)],
            0,
            false,
        );
        r.pump().await;
        assert!(r.handle.flashing());
        assert_eq!(r.take(), ["changed writing Router firmware: 0 %"]);
        r.after_flash().await;
        // The events are read after the fact, so each shows the final status: count them.
        // 5 % steps: 5..=100 (0 % came with the start), then "starting" and the role's two.
        let seen = r.take();
        let changed = seen.iter().filter(|s| s.starts_with("changed")).count();
        assert_eq!(changed, 20 + 1 + 2, "{seen:?}");
        assert_eq!(
            seen.iter().filter(|s| *s == "outcome 0x00000000").count(),
            1
        );
        assert_eq!(r.handle.status(), "router: not in a network");
        assert!(!r.handle.flashing());
        assert_eq!(*r.flashed.lock().unwrap(), 1);
        assert_eq!(r.opens(), 2);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn at_most_two_uploads_per_start() {
    local(async {
        let mut r = Rig::spawn(Mode::Router, &[Fw::Runs(GATEWAY)], 0, false);
        r.pump().await;
        r.after_flash().await;
        r.pump().await;
        assert_eq!(*r.flashed.lock().unwrap(), 2);
        let seen = r.take();
        assert_eq!(
            seen[seen.len() - 2..],
            [
                "outcome 0x50010203".to_owned(),
                "changed error: the radio does not take firmware 0x50010203".to_owned()
            ]
        );
        assert_eq!(r.opens(), 3);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_image_given_up_on_is_not_written_again() {
    local(async {
        let mut r = Rig::spawn(Mode::Router, &[Fw::Runs(GATEWAY)], ROUTER_IMAGE, false);
        r.pump().await;
        assert_eq!(
            r.take(),
            ["changed error: the radio did not take firmware 0x50010203; not written again"]
        );
        assert_eq!(*r.flashed.lock().unwrap(), 0);
        // Same family: what runs is kept.
        let mut r = Rig::spawn(Mode::Router, &[Fw::Runs(0x5000_0001)], ROUTER_IMAGE, false);
        r.pump().await;
        assert_eq!(
            r.take(),
            [
                "changed router: not in a network",
                "changed router: not in a network"
            ]
        );
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn silent_radio_is_written_or_asked_again() {
    local(async {
        // Silent for 10 s: written.
        let mut r = Rig::spawn(
            Mode::Router,
            &[Fw::Silent, Fw::Runs(ROUTER_IMAGE)],
            0,
            false,
        );
        r.pump().await;
        tokio::time::advance(Duration::from_millis(9_999)).await;
        r.pump().await;
        assert!(!r.handle.flashing());
        tokio::time::advance(Duration::from_millis(1)).await;
        r.pump().await;
        assert!(r.handle.flashing());
        r.after_flash().await;
        assert!(
            r.handle.status().starts_with("router: "),
            "{}",
            r.handle.status()
        );

        // Silent, and its image was given up on: asked again after 60 s.
        let mut r = Rig::spawn(Mode::Router, &[Fw::Silent], ROUTER_IMAGE, false);
        r.pump().await;
        tokio::time::advance(Duration::from_millis(10_000)).await;
        r.pump().await;
        assert_eq!(
            r.handle.status(),
            "error: the radio did not take firmware 0x50010203; not written again"
        );
        tokio::time::advance(Duration::from_millis(59_999)).await;
        r.pump().await;
        assert_eq!(r.opens(), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        r.pump().await;
        assert_eq!(r.opens(), 2);
        assert_eq!(r.handle.status(), "starting");
        let _ = &r.fws;
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn coordinator_runs_with_its_deconz_server() {
    local(async {
        let mut r = Rig::spawn(Mode::Coordinator, &[Fw::Runs(COORD_IMAGE)], 0, false);
        r.pump().await;
        assert_eq!(
            r.take(),
            [
                "outcome 0x00000000",
                "changed coordinator: PAN 0x1A62, channel 15",
                "changed coordinator: PAN 0x1A62, channel 15"
            ]
        );
        drop(r.dir);
    })
    .await;
}

#[test]
fn status_texts() {
    let (ev, _rx) = unbounded_channel();
    let (flash_tx, _frx) = unbounded_channel();
    let mut s = Service {
        cfg: ZigbeeConfig {
            lines: absent_lines(),
            coordinator_image: PathBuf::new(),
            router_image: PathBuf::new(),
            deconz_port: 0,
            failed_version: 0,
        },
        mode: Mode::Coordinator,
        backend: Backend {
            open: Box::new(|_| None),
            flash: Arc::new(|_, _, _| Ok(())),
        },
        shared: Rc::new(Shared {
            status: RefCell::new(String::new()),
            flashing: Cell::new(false),
        }),
        events: ev,
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
    assert_eq!(s.status(), "off");
    s.phase = Phase::Starting;
    assert_eq!(s.status(), "starting");
    s.phase = Phase::Flashing;
    s.progress = 0.374;
    assert_eq!(s.status(), "writing Coordinator firmware: 37 %");
    s.phase = Phase::Failed;
    s.error = "cannot open the deCONZ port".into();
    assert_eq!(s.status(), "error: cannot open the deCONZ port");
    s.phase = Phase::Running;
    assert_eq!(s.status(), "coordinator: radio not ready");
    s.mode = Mode::Router;
    assert_eq!(s.status(), "router: radio not ready");
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_a_flash_does_not_wait_for_it() {
    local(async {
        let mut r = Rig::spawn(Mode::Router, &[Fw::Runs(GATEWAY)], 0, false);
        r.pump().await;
        assert!(r.handle.flashing());
        // The upload is stuck (gate closed): dropping the handles still ends the actor at once.
        drop(r.handle);
        let start = tokio::time::Instant::now();
        assert!(actors::join_all(Duration::from_secs(3)).await);
        assert_eq!(start.elapsed(), Duration::ZERO);
        // Let the detached flasher thread finish.
        *r.gate.0.lock().unwrap() = true;
        r.gate.1.notify_all();
    })
    .await;
}
