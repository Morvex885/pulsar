//! The key / switch state machine on injected evdev events, on tokio's paused clock.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use pulsar_core::model::{model_from_compatible, model_from_dt_id};
use tokio::sync::mpsc::error::TryRecvError;

use super::*;

const UP: u16 = KEY_VOLUMEUP;
const KEY_MUTE: u16 = 113;
const KEY_ACTION: u16 = 582;

fn lite() -> &'static Model {
    model_from_compatible(b"yandex,station-micro\0").unwrap()
}

fn mini() -> &'static Model {
    model_from_dt_id(b"axg_s420_v03").unwrap()
}

fn mini2() -> &'static Model {
    model_from_compatible(b"yandex,station-mini2\0").unwrap()
}

fn midi() -> &'static Model {
    model_from_compatible(b"yandex,cucumber\0").unwrap()
}

fn ev(type_: u16, code: u16, value: i32) -> RawInputEvent {
    RawInputEvent {
        type_,
        code,
        value,
        ..sys::EMPTY_EVENT
    }
}

fn key(code: u16, value: i32) -> [RawInputEvent; 2] {
    [ev(EV_KEY, code, value), ev(EV_SYN, SYN_REPORT, 0)]
}

struct Rig {
    m: Machine,
    rx: UnboundedReceiver<InputEvent>,
    keys: io::Result<KeyBits>,
    sw: io::Result<SwBits>,
}

impl Rig {
    fn new() -> Self {
        Self::for_model(midi())
    }

    fn for_model(model: &Model) -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            m: Machine::new(Rc::new(Cell::new(false)), tx, model),
            rx,
            keys: Ok([0; 96]),
            sw: Ok([0; 8]),
        }
    }

    fn feed(&mut self, dev: Dev, events: &[RawInputEvent]) {
        let keys = self
            .keys
            .as_ref()
            .map_err(|e| io::Error::from(e.kind()))
            .copied();
        let sw = self
            .sw
            .as_ref()
            .map_err(|e| io::Error::from(e.kind()))
            .copied();
        let mut keys = Some(keys);
        let mut sw = Some(sw);
        self.m
            .process(
                dev,
                7,
                events,
                &mut || keys.take().expect("one key query"),
                &mut || sw.take().expect("one switch query"),
            )
            .unwrap();
    }

    /// Runs the machine's timers for `ms` of (paused) time.
    async fn run(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            tokio::select! {
                biased;
                t = self.m.timers.next() => self.m.on_tick(t, |_| {}),
                () = tokio::time::sleep_until(end) => return,
            }
        }
    }

    /// Waits for the next timer; `queued` are key events already waiting on the device then.
    async fn tick_with(&mut self, queued: &[RawInputEvent]) {
        let t = self.m.timers.next().await;
        self.m.on_tick(t, |m| {
            m.process(
                Dev::Keys,
                7,
                queued,
                &mut || Ok([0; 96]),
                &mut || Ok([0; 8]),
            )
            .unwrap();
        });
    }

    fn events(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(e) => out.push(e),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return out,
            }
        }
    }
}

const SHORT: InputEvent = InputEvent::Button(Button::Action, Press::Short);
const LONG: InputEvent = InputEvent::Button(Button::Action, Press::Long);
const VOL_UP: InputEvent = InputEvent::Button(Button::VolumeUp, Press::Short);

#[tokio::test(start_paused = true)]
async fn mute_key_toggles_once_per_press() {
    let mut r = Rig::for_model(mini());
    r.feed(Dev::Mute, &key(KEY_MUTE, 1));
    assert!(r.events().is_empty());
    r.feed(Dev::Keys, &key(KEY_MUTE, 1));
    assert_eq!(r.events(), [InputEvent::MuteToggle]);
    // Autorepeat and release are not new presses.
    r.feed(Dev::Keys, &key(KEY_MUTE, 2));
    r.feed(Dev::Keys, &key(KEY_MUTE, 0));
    assert_eq!(r.events(), []);
    r.feed(Dev::Keys, &key(KEY_MUTE, 1));
    assert_eq!(r.events(), [InputEvent::MuteToggle]);

    // A model without a mute button ignores KEY_MUTE on its keys device.
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(KEY_MUTE, 1));
    r.feed(Dev::Keys, &key(KEY_MUTE, 0));
    assert_eq!(r.events(), []);
}

#[tokio::test(start_paused = true)]
async fn mute_key_on_its_own_device() {
    // Лайт: KEY_MUTE приходит с отдельного устройства-кнопки, а не с сенсорных кнопок.
    let mut r = Rig::for_model(lite());
    r.feed(Dev::Keys, &key(KEY_MUTE, 1));
    r.feed(Dev::Mute, &key(UP, 1));
    assert!(r.events().is_empty());
    r.feed(Dev::Mute, &key(KEY_MUTE, 1));
    assert_eq!(r.events(), [InputEvent::MuteToggle]);
    r.feed(Dev::Mute, &key(KEY_MUTE, 0));
    assert_eq!(r.events(), []);
    // Потерянные события у кнопки не восстанавливаются: состояние переключателя не запрашивается.
    // Rig отдал бы «микрофоны выключены» (бит SW_MUTE_DEVICE): запрос включил бы mute и послал
    // событие Mute.
    let mut sw = [0u8; 8];
    sw[usize::from(SW_MUTE_DEVICE / 8)] |= 1 << (SW_MUTE_DEVICE % 8);
    r.sw = Ok(sw);
    r.feed(
        Dev::Mute,
        &[ev(EV_SYN, SYN_DROPPED, 0), ev(EV_SYN, SYN_REPORT, 0)],
    );
    // EV_SW с устройства-кнопки тоже не источник состояния.
    r.feed(
        Dev::Mute,
        &[ev(EV_SW, SW_MUTE_DEVICE, 1), ev(EV_SYN, SYN_REPORT, 0)],
    );
    assert_eq!(r.events(), []);
    assert!(!r.m.muted.get());

    // У переключателя (Миди и др.) его KEY_MUTE по-прежнему игнорируется.
    let mut r = Rig::new();
    r.feed(Dev::Mute, &key(KEY_MUTE, 1));
    r.feed(Dev::Mute, &key(KEY_MUTE, 0));
    assert_eq!(r.events(), []);
}

#[tokio::test(start_paused = true)]
async fn no_action_key_leaves_key_voicecommand_unhandled() {
    // The Mini: mute button only.
    let mut r = Rig::for_model(mini());
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(1000).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), []);
    r.feed(Dev::Keys, &key(KEY_MUTE, 1));
    assert_eq!(r.events(), [InputEvent::MuteToggle]);
}

#[tokio::test(start_paused = true)]
async fn action_long_fires_while_held_and_the_release_adds_nothing() {
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(799).await;
    assert_eq!(r.events(), []);
    r.run(1).await;
    assert_eq!(r.events(), [LONG]);
    r.run(2000).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), []);
}

#[tokio::test(start_paused = true)]
async fn action_short_needs_56_ms_and_300_ms_gap() {
    let mut r = Rig::new();
    // Too short: capacitive noise.
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(55).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), []);
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(56).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), [SHORT]);
    // Released 299 ms after the accepted click (input.cpp: now - last < 300): dropped.
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(299).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), []);
    // The dropped click does not move the gap: released 244 ms after it (543 ms after the
    // accepted one), the next click passes.
    r.run(144).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(100).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), [SHORT]);

    // Exactly 300 ms after the accepted click: accepted.
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(100).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(300).await;
    r.feed(Dev::Keys, &key(KEY_ACTION, 0));
    assert_eq!(r.events(), [SHORT, SHORT]);
}

#[tokio::test(start_paused = true)]
async fn queued_events_go_before_a_timer_of_the_same_wake_up() {
    // Released exactly when the long press comes due: the release wins (short), as in C++.
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.tick_with(&key(KEY_ACTION, 0)).await;
    assert_eq!(r.events(), [SHORT]);
    r.run(20_000).await;
    assert_eq!(r.events(), []);

    // Volume released when the repeat delay comes due: no repeat.
    r.feed(Dev::Keys, &key(UP, 1));
    assert_eq!(r.events(), [VOL_UP]);
    r.tick_with(&key(UP, 0)).await;
    r.run(2000).await;
    assert_eq!(r.events(), []);

    // ...and when a repeat comes due.
    r.feed(Dev::Keys, &key(UP, 1));
    r.run(500).await;
    assert_eq!(r.events(), [VOL_UP, VOL_UP]);
    r.tick_with(&key(UP, 0)).await;
    r.run(2000).await;
    assert_eq!(r.events(), []);

    // An unrelated event first, then the timer still fires.
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.tick_with(&key(KEY_VOLUMEDOWN, 1)).await;
    assert_eq!(
        r.events(),
        [InputEvent::Button(Button::VolumeDown, Press::Short), LONG]
    );
}

#[tokio::test(start_paused = true)]
async fn volume_repeats_after_500_ms_every_200_ms_until_release() {
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(UP, 1));
    assert_eq!(r.events(), [VOL_UP]);
    r.run(499).await;
    assert_eq!(r.events(), []);
    r.run(1).await;
    assert_eq!(r.events(), [VOL_UP]);
    r.run(600).await;
    assert_eq!(r.events(), [VOL_UP; 3]);
    // Autorepeat (value 2) and the other key's release change nothing.
    r.feed(Dev::Keys, &key(UP, 2));
    r.feed(Dev::Keys, &key(KEY_VOLUMEDOWN, 0));
    r.run(100).await;
    r.feed(Dev::Keys, &key(UP, 0));
    r.run(2000).await;
    assert_eq!(r.events(), []);
}

#[tokio::test(start_paused = true)]
async fn stuck_keys_are_released_after_10_s() {
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(UP, 1));
    r.run(20_000).await;
    // Press, 500 ms, then every 200 ms while held <= 10 s: 500, 700, .., 9900.
    assert_eq!(r.events().len(), 1 + 48);
    assert!(r.m.repeat_code.is_none());
    assert!(!r.m.timers.is_set(Tick::Repeat));

    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(10_000).await;
    assert_eq!(r.events(), [LONG]);
    assert!(r.m.action_down.is_none());
    // The guard lets the next press through although the release never came.
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(800).await;
    assert_eq!(r.events(), [LONG]);
}

#[tokio::test(start_paused = true)]
async fn syn_dropped_resyncs_keys_from_the_device() {
    let mut r = Rig::new();
    r.feed(Dev::Keys, &key(KEY_ACTION, 1));
    r.run(100).await;
    // The release is lost in the overflow; the key state says nothing is held.
    r.feed(
        Dev::Keys,
        &[
            ev(EV_SYN, SYN_DROPPED, 0),
            ev(EV_KEY, KEY_ACTION, 0),
            ev(EV_KEY, UP, 1),
        ],
    );
    assert_eq!(r.events(), []);
    r.feed(Dev::Keys, &[ev(EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(r.events(), [SHORT]);
    assert!(!r.m.keys_dropped);

    // Still held according to the device: nothing synthesized.
    r.feed(Dev::Keys, &key(UP, 1));
    assert_eq!(r.events(), [VOL_UP]);
    let mut held = [0u8; 96];
    held[usize::from(UP / 8)] |= 1 << (UP % 8);
    r.keys = Ok(held);
    r.feed(Dev::Keys, &[ev(EV_SYN, SYN_DROPPED, 0)]);
    r.feed(Dev::Keys, &[ev(EV_SYN, SYN_REPORT, 0)]);
    assert_eq!(r.m.repeat_code, Some(UP));

    // EVIOCGKEY failing forgets every key.
    r.keys = Err(io::ErrorKind::NotFound.into());
    r.feed(Dev::Keys, &[ev(EV_SYN, SYN_DROPPED, 0)]);
    r.feed(Dev::Keys, &[ev(EV_SYN, SYN_REPORT, 0)]);
    assert!(r.m.repeat_code.is_none());
    r.run(1000).await;
    assert_eq!(r.events(), []);
}

#[tokio::test(start_paused = true)]
async fn mute_switch_and_its_resync() {
    let mut r = Rig::new();
    assert!(r.m.muted.get());
    r.m.read_mute_state(Ok([0; 8]), true).unwrap();
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(false)]);

    r.feed(Dev::Keys, &[ev(EV_SW, SW_MUTE_DEVICE, 1)]);
    assert!(!r.m.muted.get());
    assert!(r.events().is_empty());
    r.feed(Dev::Mute, &[ev(EV_SW, SW_MUTE_DEVICE, 1)]);
    assert_eq!(r.events(), [InputEvent::Mute(true)]);
    r.feed(
        Dev::Mute,
        &[ev(EV_SW, SW_MUTE_DEVICE, 0), ev(EV_SYN, SYN_REPORT, 0)],
    );
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::Mute(false)]);
    // KEY_MUTE of the same device is ignored.
    r.feed(Dev::Mute, &key(113, 1));
    assert_eq!(r.events(), []);

    r.sw = Ok([0; 8]);
    r.feed(
        Dev::Mute,
        &[ev(EV_SYN, SYN_DROPPED, 0), ev(EV_SW, SW_MUTE_DEVICE, 0)],
    );
    assert!(r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(true)]);
    r.feed(Dev::Mute, &[ev(EV_SYN, SYN_REPORT, 0)]);
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(false)]);
}

#[tokio::test(start_paused = true)]
async fn switch_resync_does_not_replay_events_older_than_ioctl() {
    let mut r = Rig::new();
    r.m.read_mute_state(Ok([0; 8]), false).unwrap();
    let mut on = [0; 8];
    on[1] = 0x40;
    r.sw = Ok(on);
    r.feed(
        Dev::Mute,
        &[
            ev(EV_SYN, SYN_DROPPED, 0),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_SW, SW_MUTE_DEVICE, 0),
            ev(EV_SYN, SYN_REPORT, 0),
            ev(EV_SW, SW_MUTE_DEVICE, 1),
            ev(EV_SYN, SYN_REPORT, 0),
        ],
    );
    assert!(r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(true)]);

    r.feed(
        Dev::Mute,
        &[ev(EV_SW, SW_MUTE_DEVICE, 0), ev(EV_SYN, SYN_REPORT, 0)],
    );
    assert_eq!(r.events(), [InputEvent::Mute(false)]);
}

#[test]
fn event_device_by_name() {
    let devices = "I: Bus=0019 Vendor=0001 Product=0001 Version=0100\n\
                   N: Name=\"gpio-keys\"\n\
                   P: Phys=gpio-keys/input0\n\
                   H: Handlers=kbd event1 \n\
                   B: SW=4000\n\
                   \n\
                   I: Bus=0018 Vendor=0000 Product=0000 Version=0000\n\
                   N: Name=\"aw9610x\"\n\
                   H: Handlers=event2\n\
                   \n\
                   N: Name=\"no-handler\"\n\
                   \n\
                   H: Handlers=event9\n";
    let find = |name| try_named(devices, &[name], |_, handler| Some(handler.to_owned()));
    assert_eq!(find("gpio-keys").as_deref(), Some("event1"));
    assert_eq!(find("aw9610x").as_deref(), Some("event2"));
    assert_eq!(find("no-handler"), None);
    assert_eq!(find("aw9610"), None);
}

#[test]
fn lite_tries_only_named_devices_in_model_order() {
    const UNKNOWN: &str = "N: Name=\"other\"\nH: Handlers=event0\n\n";
    const CYPRESS: &str = "N: Name=\"cy8cmbr3108\"\nH: Handlers=event1\n\n";
    const SAR: &str = "N: Name=\"sar0_pad0\"\nH: Handlers=event2\n\n";
    let choose = |devices, broken| {
        try_named(devices, lite().keys_input, |_, handler| {
            (handler != broken).then(|| handler.to_owned())
        })
    };
    let both = [UNKNOWN, CYPRESS, SAR].concat();
    assert_eq!(choose(CYPRESS, "").as_deref(), Some("event1"));
    assert_eq!(choose(&both, "").as_deref(), Some("event2"));
    assert_eq!(choose(&both, "event2").as_deref(), Some("event1"));
    assert_eq!(choose(UNKNOWN, ""), None);
}

fn socket_evdev() -> (Evdev, UnixStream) {
    let (read, write) = UnixStream::pair().unwrap();
    read.set_nonblocking(true).unwrap();
    (
        Evdev::from_test_file(File::from(OwnedFd::from(read))),
        write,
    )
}

fn write_event(writer: &mut UnixStream, event: RawInputEvent) {
    let mut bytes = [0u8; size_of::<RawInputEvent>()];
    let data = &mut bytes[std::mem::offset_of!(RawInputEvent, type_)..];
    data[..2].copy_from_slice(&event.type_.to_ne_bytes());
    data[2..4].copy_from_slice(&event.code.to_ne_bytes());
    data[4..].copy_from_slice(&event.value.to_ne_bytes());
    writer.write_all(&bytes).unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_lite_mute_key_survives_empty_start() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let present = Rc::new(Cell::new(false));
            let (fd, mut writer) = socket_evdev();
            let fd = Cell::new(Some(fd));
            let (handle, mut events) = spawn_input_with(
                lite(),
                {
                    let present = Rc::clone(&present);
                    move || {
                        if present.get() {
                            "N: Name=\"gpio-keys-filter\"\nH: Handlers=event1\n".into()
                        } else {
                            String::new()
                        }
                    }
                },
                move |path| {
                    assert_eq!(path, "/dev/input/event1");
                    fd.take().ok_or_else(|| io::ErrorKind::NotFound.into())
                },
            );
            assert!(!handle.is_available());
            assert!(!handle.hw_muted());
            present.set(true);
            write_event(&mut writer, ev(EV_KEY, KEY_MUTE, 1));
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(REOPEN_MS + 100), events.recv())
                    .await
                    .unwrap(),
                Some(InputEvent::MuteToggle)
            );
            assert!(handle.is_available());
            drop(writer);
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert!(!handle.is_available());
            assert!(!handle.hw_muted());
            assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
        })
        .await;
}

#[tokio::test(start_paused = true)]
async fn failed_switch_resync_drops_fd_and_retries() {
    let mut r = Rig::new();
    let mut devs = Devices::empty(midi(), Rc::new(Cell::new(false)));
    let (fd, mut writer) = socket_evdev();
    devs.mute = register_dev(fd, "mute", "/dev/input/event1");
    r.m.read_mute_state(Ok([0; 8]), false).unwrap();
    assert!(!r.m.muted.get());
    assert!(r.events().is_empty());
    write_event(&mut writer, ev(EV_SYN, SYN_DROPPED, 0));
    write_event(&mut writer, ev(EV_SYN, SYN_REPORT, 0));
    drain(
        &mut r.m,
        &mut devs,
        Dev::Mute,
        &mut [sys::EMPTY_EVENT; 16],
        Read::Now,
    );
    assert!(devs.mute.is_none());
    assert!(r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(true)]);

    for _ in 0..2 {
        let (fd, _writer) = socket_evdev();
        let fd = Cell::new(Some(fd));
        devs.open_missing(
            &mut r.m,
            || "N: Name=\"mute\"\nH: Handlers=event1\n".into(),
            |_| Ok(fd.take().expect("missing switch is opened again")),
            false,
        );
        assert!(fd.take().is_none());
        assert!(devs.mute.is_none());
        assert!(r.m.muted.get());
        assert!(r.events().is_empty());
    }
    r.m.read_mute_state(Ok([0; 8]), true).unwrap();
    assert_eq!(r.events(), [InputEvent::MuteSync(false)]);
}

#[tokio::test(start_paused = true)]
async fn mute_gpio_is_active_low_and_fails_closed() {
    let dir = std::env::temp_dir().join(format!("pulsar-hw-gpio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let line = dir.join("gpio414");
    std::fs::create_dir_all(&line).unwrap();
    let d = dir.to_str().unwrap();
    // Neither the line nor an export file: nothing.
    assert!(open_mute_gpio(d, 415, true).is_none());
    // An output is never taken over (nor reconfigured).
    std::fs::write(line.join("value"), "0\n").unwrap();
    std::fs::write(line.join("direction"), "out\n").unwrap();
    assert!(open_mute_gpio(d, 414, true).is_none());
    assert_eq!(
        std::fs::read_to_string(line.join("direction")).unwrap(),
        "out\n"
    );
    let mut r = Rig::for_model(mini2());
    let mut devs = Devices::empty(mini2(), Rc::new(Cell::new(false)));
    assert!(r.m.muted.get());
    devs.open_gpio(&mut r.m, d, true);
    assert!(devs.mute_gpio.is_none());
    std::fs::write(line.join("direction"), "in\n").unwrap();
    let value = line.join("value");
    std::fs::write(&value, "1\n").unwrap();
    devs.open_gpio(&mut r.m, d, false);
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(false)]);
    for dev in [Dev::Keys, Dev::Mute] {
        r.feed(dev, &[ev(EV_SW, SW_MUTE_DEVICE, 1)]);
    }
    assert!(r.events().is_empty());
    assert!(!r.m.muted.get());

    std::fs::write(&value, "0\n").unwrap();
    devs.poll_mute_gpio(&mut r.m);
    assert_eq!(r.events(), [InputEvent::Mute(true)]);
    std::fs::write(&value, "1\n").unwrap();
    devs.poll_mute_gpio(&mut r.m);
    assert_eq!(r.events(), [InputEvent::Mute(false)]);
    std::fs::write(&value, "").unwrap();
    devs.poll_mute_gpio(&mut r.m);
    assert!(devs.mute_gpio.is_none());
    assert!(r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(true)]);
    devs.open_gpio(&mut r.m, d, false);
    assert!(devs.mute_gpio.is_none());
    assert!(r.m.muted.get());
    assert!(r.events().is_empty());
    std::fs::write(&value, "1\n").unwrap();
    devs.open_gpio(&mut r.m, d, false);
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::MuteSync(false)]);
    std::fs::remove_dir_all(&dir).unwrap();
}
