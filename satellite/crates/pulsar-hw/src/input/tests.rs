//! The key / switch state machine on injected evdev events, on tokio's paused clock.

use std::time::Duration;

use tokio::sync::mpsc::error::TryRecvError;

use super::*;

const UP: u16 = KEY_VOLUMEUP;

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
        let (tx, rx) = unbounded_channel();
        Self {
            m: Machine::new(Rc::new(Cell::new(false)), tx),
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
        self.m.process(
            dev,
            7,
            events,
            &mut || keys.take().expect("one key query"),
            &mut || sw.take().expect("one switch query"),
        );
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
            );
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
    let mut on = [0u8; 8];
    on[1] = 0x40; // SW_MUTE_DEVICE = bit 14
    // The state read at spawn changes hw_muted without an event.
    r.m.read_mute_state(Ok(on), false);
    assert!(r.m.muted.get());
    assert_eq!(r.events(), []);

    r.feed(
        Dev::Mute,
        &[ev(EV_SW, SW_MUTE_DEVICE, 0), ev(EV_SYN, SYN_REPORT, 0)],
    );
    assert!(!r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::Mute(false)]);
    // KEY_MUTE of the same device is ignored.
    r.feed(Dev::Mute, &key(113, 1));
    assert_eq!(r.events(), []);

    r.sw = Ok(on);
    r.feed(
        Dev::Mute,
        &[ev(EV_SYN, SYN_DROPPED, 0), ev(EV_SW, SW_MUTE_DEVICE, 0)],
    );
    assert_eq!(r.events(), []);
    r.feed(Dev::Mute, &[ev(EV_SYN, SYN_REPORT, 0)]);
    assert!(r.m.muted.get());
    assert_eq!(r.events(), [InputEvent::Mute(true)]);
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
    assert_eq!(
        find_event_dev_in(devices, "gpio-keys").as_deref(),
        Some("/dev/input/event1")
    );
    assert_eq!(
        find_event_dev_in(devices, "aw9610x").as_deref(),
        Some("/dev/input/event2")
    );
    // A blank line ends the device block.
    assert_eq!(find_event_dev_in(devices, "no-handler"), None);
    assert_eq!(find_event_dev_in(devices, "aw9610"), None);
}

#[test]
fn mute_gpio_is_an_active_low_input() {
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
    std::fs::write(line.join("direction"), "in\n").unwrap();
    let f = open_mute_gpio(d, 414, true).unwrap();
    assert!(gpio_muted(&f).unwrap());
    std::fs::write(line.join("value"), "1\n").unwrap();
    assert!(!gpio_muted(&f).unwrap());
    std::fs::remove_dir_all(&dir).unwrap();
}
