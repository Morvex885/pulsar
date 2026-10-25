//! Buttons via evdev (devices matched by name in /proc/bus/input/devices, names per Model).
//! Station Midi: touch keys "awinic_sar", mute switch "mute"; Station Lite 2: touch keys
//! "aw9610x", mute switch "gpio-keys"; Station Mini 2: touch keys "sar0_pad0", no mute switch
//! device: the latched mute line is a GPIO (`Model::mute_gpio`, low = muted), sampled every
//! 100 ms. All use the same key codes and switch:
//!
//! touch keys (capacitive, EV_KEY only; Midi KEY bitmap "40 0 0 0 0 0 0 0 c000000000000 0"):
//!   KEY_VOICECOMMAND (582) -> Action     (maind: "ACTIVATE")
//!   KEY_VOLUMEUP     (115) -> VolumeUp
//!   KEY_VOLUMEDOWN   (114) -> VolumeDown
//! mute switch (gpio-keys, KEY bitmap bit 113 KEY_MUTE, SW bitmap 0x4000 = SW_MUTE_DEVICE (14)):
//!   the hardware microphone mute slider/toggle; SW_MUTE_DEVICE=1 means muted. Its KEY_MUTE
//!   press/release events are ignored, the switch state is authoritative (initial value via
//!   EVIOCGSW).
//! Action: Short on release before 800 ms, Long as soon as it has been held for 800 ms.
//! Volume*: Short on press, auto-repeated every 200 ms after being held for 500 ms, for at most
//! 10 s.
//! Devices that disappear (ENODEV) are looked up again every 3 s; SYN_DROPPED triggers a state
//! resync.

use std::cell::Cell;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::rc::Rc;

use log::{debug, error, info, warn};
use pulsar_core::Timers;
use pulsar_core::model::Model;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::Instant;

use crate::sys::{self, Evdev, KeyBits, RawInputEvent, SwBits};

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_SW: u16 = 0x05;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const SW_MUTE_DEVICE: u16 = 0x0e;
/// KEY_VOICECOMMAND
const KEY_ACTION: u16 = 582;
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;

const LONG_PRESS_MS: u64 = 800;
const REPEAT_DELAY_MS: u64 = 500;
const REPEAT_MS: u64 = 200;
/// A key held longer than this is taken as stuck.
const MAX_HOLD_MS: u64 = 10000;
/// Retry period for missing/vanished input devices.
const REOPEN_MS: u64 = 3000;
const GPIO_DIR: &str = "/sys/class/gpio";
/// Sampling period of the mute GPIO (maind polls it too).
const GPIO_POLL_MS: u64 = 100;
// maind's buttonsClickLimiter for "activate" (quasar.cfg): touches shorter than 56 ms and clicks
// closer than 300 ms to the previous one are dropped.
const MIN_ACTION_PRESS_MS: i64 = 56;
const MIN_ACTION_GAP_MS: i64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Action,
    VolumeUp,
    VolumeDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Short,
    Long,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Button(Button, Press),
    /// Hardware mic mute switch changed (true = muted).
    Mute(bool),
}

/// Keeps the input actor alive; the actor stops when the last clone is dropped.
#[derive(Clone)]
pub struct InputHandle {
    muted: Rc<Cell<bool>>,
    _alive: UnboundedSender<()>,
}

impl InputHandle {
    /// State of the mute switch: read at spawn (without a `Mute` event), then kept by the
    /// actor before each `Mute` event and on every reopen.
    pub fn hw_muted(&self) -> bool {
        self.muted.get()
    }
}

/// Opens the touch keys (`Model::keys_input`) and the mute switch (`Model::mute_input`, or the
/// GPIO `Model::mute_gpio`); Err when none opened. Missing or vanished devices are looked up
/// again every 3 s.
pub fn spawn_input(
    model: &'static Model,
) -> Result<(InputHandle, UnboundedReceiver<InputEvent>), String> {
    let keys = open_dev(model.keys_input, false);
    let mute = if model.mute_input.is_empty() {
        None
    } else {
        open_dev(model.mute_input, false)
    };
    let mute_gpio = if model.mute_gpio == 0 {
        None
    } else {
        open_mute_gpio(GPIO_DIR, model.mute_gpio, false)
    };
    if keys.is_none() && mute.is_none() && mute_gpio.is_none() {
        return Err("input devices unavailable".to_owned());
    }
    let muted = Rc::new(Cell::new(false));
    let (tx, rx) = unbounded_channel();
    let mut m = Machine::new(Rc::clone(&muted), tx);
    // The state the controller starts from: no event for it.
    if let Some(fd) = &mute {
        m.read_mute_state(fd.get_ref().switch_state(), false);
    }
    if let Some(f) = &mute_gpio
        && let Ok(v) = gpio_muted(f)
    {
        m.set_muted(v, false);
    }
    if mute.is_some() || mute_gpio.is_some() {
        info!(
            "input: initial hardware mute {}",
            if muted.get() { "on" } else { "off" }
        );
    }
    m.arm_repeating(Tick::Reopen, REOPEN_MS, REOPEN_MS);
    if mute_gpio.is_some() {
        m.arm_repeating(Tick::MuteGpio, GPIO_POLL_MS, GPIO_POLL_MS);
    }
    let (alive_tx, alive_rx) = unbounded_channel();
    let devs = Devices {
        model,
        keys,
        mute,
        mute_gpio,
    };
    pulsar_core::actors::spawn_actor("input", run(m, devs, alive_rx));
    Ok((
        InputHandle {
            muted,
            _alive: alive_tx,
        },
        rx,
    ))
}

/// /dev/input/eventN of the device with the given name in a /proc/bus/input/devices listing.
fn find_event_dev_in(devices: &str, name: &str) -> Option<String> {
    let want = format!("N: Name=\"{name}\"");
    let mut matched = false;
    for line in devices.split('\n') {
        if line.starts_with("N: Name=") {
            matched = line == want;
        } else if matched && let Some(handlers) = line.strip_prefix("H: Handlers=") {
            if let Some(ev) = handlers.split_whitespace().find(|t| t.starts_with("event")) {
                return Some(format!("/dev/input/{ev}"));
            }
        } else if line.is_empty() {
            matched = false;
        }
    }
    None
}

fn open_dev(name: &str, quiet: bool) -> Option<AsyncFd<Evdev>> {
    let devices = pulsar_core::sysfs::read_file("/proc/bus/input/devices").unwrap_or_default();
    let Some(path) = find_event_dev_in(&devices, name) else {
        if !quiet {
            error!("input: device \"{name}\" not found");
        }
        return None;
    };
    let dev = match Evdev::open(&path) {
        Ok(d) => d,
        Err(e) => {
            if !quiet {
                error!("input: open {path}: {}", crate::errstr(&e));
            }
            return None;
        }
    };
    match dev.into_async() {
        Ok(fd) => {
            info!("input: \"{name}\" -> {path}");
            Some(fd)
        }
        Err(e) => {
            error!("input: poll {path}: {}", crate::errstr(&e));
            None
        }
    }
}

/// The value file of mute line `gpio` under `dir` (/sys/class/gpio), exported if nobody did. A
/// global GPIO number only names this pin on the kernel it was read from (on the Midi, 475 is
/// Wi-Fi SDIO), so the line is never configured: no `direction`, no `edge` write, and it is used
/// only when it already is an input.
fn open_mute_gpio(dir: &str, gpio: u32, quiet: bool) -> Option<File> {
    let line = format!("{dir}/gpio{gpio}");
    if !Path::new(&line).exists() {
        pulsar_core::sysfs::write_file(&format!("{dir}/export"), &gpio.to_string());
    }
    let direction = pulsar_core::sysfs::read_file(&format!("{line}/direction")).unwrap_or_default();
    let direction = pulsar_core::sysfs::trim(&direction);
    if direction != "in" {
        if !quiet {
            warn!("input: gpio{gpio} is not an input ('{direction}'), no mute switch");
        }
        return None;
    }
    let path = format!("{line}/value");
    // A value file that opens but cannot be read is no switch either.
    let opened = File::open(&path).and_then(|f| gpio_muted(&f).map(|_| f));
    match opened {
        Ok(f) => {
            info!("input: mute switch on gpio{gpio}");
            Some(f)
        }
        Err(e) => {
            if !quiet {
                error!("input: {path}: {}", crate::errstr(&e));
            }
            None
        }
    }
}

/// Mute line level: pulled up, held low by the mic circuit while the microphones are cut.
fn gpio_muted(f: &File) -> io::Result<bool> {
    let mut b = [0u8; 1];
    if f.read_at(&mut b, 0)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(b[0] == b'0')
}

fn test_bit(bits: &[u8], n: u16) -> bool {
    bits.get(usize::from(n / 8))
        .is_some_and(|b| (b >> (n % 8)) & 1 == 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum Tick {
    /// Long press, then the stuck-key guard.
    Action,
    RepeatDelay,
    Repeat,
    Reopen,
    /// Sampling of the mute GPIO.
    MuteGpio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dev {
    Keys,
    Mute,
}

/// Key and switch state machine, fed with evdev events and its own timers.
struct Machine {
    timers: Timers<Tick>,
    epoch: Instant,
    muted: Rc<Cell<bool>>,
    tx: UnboundedSender<InputEvent>,
    /// Press time; None = not pressed.
    action_down: Option<i64>,
    last_action_ms: Option<i64>,
    action_long_sent: bool,
    repeat_code: Option<u16>,
    repeat_since: i64,
    // After SYN_DROPPED the kernel discards events up to the next SYN_REPORT; the state is then
    // read back from the device.
    keys_dropped: bool,
    mute_dropped: bool,
    /// Per `Tick`: bumped whenever the timer is armed or cancelled.
    generation: [u64; 5],
}

fn volume_button(code: u16) -> Button {
    if code == KEY_VOLUMEUP {
        Button::VolumeUp
    } else {
        Button::VolumeDown
    }
}

impl Machine {
    fn new(muted: Rc<Cell<bool>>, tx: UnboundedSender<InputEvent>) -> Self {
        Self {
            timers: Timers::new(),
            epoch: Instant::now(),
            muted,
            tx,
            action_down: None,
            last_action_ms: None,
            action_long_sent: false,
            repeat_code: None,
            repeat_since: 0,
            keys_dropped: false,
            mute_dropped: false,
            generation: [0; 5],
        }
    }

    fn arm(&mut self, t: Tick, ms: u64) {
        self.generation[t as usize] += 1;
        self.timers.set(t, ms);
    }

    fn arm_repeating(&mut self, t: Tick, delay_ms: u64, period_ms: u64) {
        self.generation[t as usize] += 1;
        self.timers.set_repeating(t, delay_ms, period_ms);
    }

    fn disarm(&mut self, t: Tick) {
        self.generation[t as usize] += 1;
        self.timers.cancel(t);
    }

    /// A due key timer. The C++ loop handled fd events before the timers of the same wake-up,
    /// so `queued` first takes the events the devices already have (without waiting); a timer
    /// they cancelled or re-armed (a release, a new press) does not fire.
    fn on_tick(&mut self, t: Tick, queued: impl FnOnce(&mut Machine)) {
        let generation = self.generation[t as usize];
        queued(self);
        if self.generation[t as usize] == generation {
            self.on_timer(t);
        }
    }

    fn now(&self) -> i64 {
        i64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(i64::MAX)
    }

    fn emit(&self, b: Button, p: Press) {
        let name = match b {
            Button::Action => "action",
            Button::VolumeUp => "volume up",
            Button::VolumeDown => "volume down",
        };
        let press = match p {
            Press::Long => "long",
            Press::Short => "short",
        };
        info!("input: {name} {press}");
        let _ = self.tx.send(InputEvent::Button(b, p));
    }

    /// Forgets every pressed key (device lost or events dropped): no release will ever arrive.
    fn release_all(&mut self) {
        self.disarm(Tick::Action);
        self.disarm(Tick::RepeatDelay);
        self.disarm(Tick::Repeat);
        self.action_down = None;
        self.repeat_code = None;
    }

    fn on_key(&mut self, code: u16, value: i32) {
        let now = self.now();
        if code == KEY_ACTION {
            if value == 1 && self.action_down.is_none() {
                self.action_down = Some(now);
                self.action_long_sent = false;
                self.arm(Tick::Action, LONG_PRESS_MS);
            } else if value == 0
                && let Some(down) = self.action_down
            {
                let held = now - down;
                self.action_down = None;
                self.disarm(Tick::Action);
                if self.action_long_sent {
                    return;
                }
                // Capacitive noise (a hand near the top, flipping the mute switch) produces very
                // short touches, often in bursts; maind's "buttonsClickLimiter" drops the same.
                if held < MIN_ACTION_PRESS_MS
                    || self
                        .last_action_ms
                        .is_some_and(|l| now - l < MIN_ACTION_GAP_MS)
                {
                    info!("input: action touch ignored (held {held} ms)");
                    return;
                }
                self.last_action_ms = Some(now);
                self.emit(Button::Action, Press::Short);
            }
            return;
        }
        if code != KEY_VOLUMEUP && code != KEY_VOLUMEDOWN {
            debug!("input: unhandled key {code}={value}");
            return;
        }
        if value == 1 {
            self.emit(volume_button(code), Press::Short);
            self.disarm(Tick::Repeat);
            self.repeat_code = Some(code);
            self.repeat_since = now;
            self.arm(Tick::RepeatDelay, REPEAT_DELAY_MS);
        } else if value == 0 && self.repeat_code == Some(code) {
            self.disarm(Tick::RepeatDelay);
            self.disarm(Tick::Repeat);
            self.repeat_code = None;
        }
    }

    /// Handles a key timer (`Tick::Reopen` and `Tick::MuteGpio` belong to the actor).
    fn on_timer(&mut self, t: Tick) {
        match t {
            Tick::Action => {
                if self.action_long_sent {
                    self.action_down = None;
                    warn!(
                        "input: action key held > {} s, assuming released",
                        MAX_HOLD_MS / 1000
                    );
                    return;
                }
                self.action_long_sent = true;
                self.emit(Button::Action, Press::Long);
                // Stuck-key guard: a release that never arrives must not block later presses.
                self.arm(Tick::Action, MAX_HOLD_MS - LONG_PRESS_MS);
            }
            Tick::RepeatDelay => {
                let Some(code) = self.repeat_code else { return };
                self.arm_repeating(Tick::Repeat, REPEAT_MS, REPEAT_MS);
                self.emit(volume_button(code), Press::Short);
            }
            Tick::Repeat => {
                let Some(code) = self.repeat_code else { return };
                if self.now() - self.repeat_since > MAX_HOLD_MS as i64 {
                    warn!(
                        "input: volume key held > {} s, assuming released",
                        MAX_HOLD_MS / 1000
                    );
                    self.disarm(Tick::Repeat);
                    self.repeat_code = None;
                    return;
                }
                self.emit(volume_button(code), Press::Short);
            }
            Tick::Reopen | Tick::MuteGpio => {}
        }
    }

    /// `notify`: send `Mute` on a change (false only for the state read at spawn).
    fn set_muted(&mut self, m: bool, notify: bool) {
        if m == self.muted.get() {
            return;
        }
        self.muted.set(m);
        info!("input: hardware mute {}", if m { "on" } else { "off" });
        if notify {
            let _ = self.tx.send(InputEvent::Mute(m));
        }
    }

    fn read_mute_state(&mut self, bits: io::Result<SwBits>, notify: bool) {
        match bits {
            Ok(b) => self.set_muted(test_bit(&b, SW_MUTE_DEVICE), notify),
            Err(e) => warn!("input: EVIOCGSW: {}", crate::errstr(&e)),
        }
    }

    /// Key state after lost events: synthesize the releases we missed (presses are not
    /// invented).
    fn resync_keys(&mut self, bits: io::Result<KeyBits>) {
        let b = match bits {
            Ok(b) => b,
            Err(e) => {
                warn!("input: EVIOCGKEY: {}", crate::errstr(&e));
                self.release_all();
                return;
            }
        };
        if self.action_down.is_some() && !test_bit(&b, KEY_ACTION) {
            self.on_key(KEY_ACTION, 0);
        }
        if let Some(code) = self.repeat_code
            && !test_bit(&b, code)
        {
            self.on_key(code, 0);
        }
    }

    /// Events read from `dev` (fd for the log); the state queries run on SYN_REPORT after a
    /// SYN_DROPPED.
    fn process(
        &mut self,
        dev: Dev,
        fd: RawFd,
        events: &[RawInputEvent],
        key_state: &mut dyn FnMut() -> io::Result<KeyBits>,
        switch_state: &mut dyn FnMut() -> io::Result<SwBits>,
    ) {
        for e in events {
            let dropped = match dev {
                Dev::Keys => self.keys_dropped,
                Dev::Mute => self.mute_dropped,
            };
            if e.type_ == EV_SYN {
                if e.code == SYN_DROPPED {
                    warn!("input: events dropped on fd {fd}, resyncing");
                    self.set_dropped(dev, true);
                } else if e.code == SYN_REPORT && dropped {
                    self.set_dropped(dev, false);
                    match dev {
                        Dev::Keys => self.resync_keys(key_state()),
                        Dev::Mute => self.read_mute_state(switch_state(), true),
                    }
                }
                continue;
            }
            if dropped {
                continue;
            }
            if e.type_ == EV_KEY && dev == Dev::Keys {
                self.on_key(e.code, e.value);
            } else if e.type_ == EV_SW && e.code == SW_MUTE_DEVICE {
                self.set_muted(e.value != 0, true);
            }
        }
    }

    fn set_dropped(&mut self, dev: Dev, v: bool) {
        match dev {
            Dev::Keys => self.keys_dropped = v,
            Dev::Mute => self.mute_dropped = v,
        }
    }
}

struct Devices {
    model: &'static Model,
    keys: Option<AsyncFd<Evdev>>,
    mute: Option<AsyncFd<Evdev>>,
    /// Value file of `Model::mute_gpio`.
    mute_gpio: Option<File>,
}

impl Devices {
    fn get(&self, dev: Dev) -> Option<&AsyncFd<Evdev>> {
        match dev {
            Dev::Keys => self.keys.as_ref(),
            Dev::Mute => self.mute.as_ref(),
        }
    }

    /// Drops a device for the reopen timer; keys still held will never be released.
    fn close(&mut self, dev: Dev, m: &mut Machine) {
        match dev {
            Dev::Keys => {
                m.release_all();
                self.keys = None;
            }
            Dev::Mute => self.mute = None,
        }
    }

    /// Devices that were missing at start or vanished (ENODEV) are looked up again.
    fn reopen_missing(&mut self, m: &mut Machine) {
        if self.keys.is_none() {
            self.keys = open_dev(self.model.keys_input, true);
            m.keys_dropped = false;
        }
        if self.mute.is_none() && !self.model.mute_input.is_empty() {
            self.mute = open_dev(self.model.mute_input, true);
            m.mute_dropped = false;
            if let Some(fd) = &self.mute {
                m.read_mute_state(fd.get_ref().switch_state(), true);
            }
        }
        if self.mute_gpio.is_none() && self.model.mute_gpio != 0 {
            self.mute_gpio = open_mute_gpio(GPIO_DIR, self.model.mute_gpio, true);
            if self.mute_gpio.is_some() {
                m.arm_repeating(Tick::MuteGpio, GPIO_POLL_MS, GPIO_POLL_MS);
                self.poll_mute_gpio(m);
            }
        }
    }

    /// Samples the mute line (sysfs value files are always readable: waiting for POLLPRI would
    /// need an `edge` written to the pin, which stays as found). A read error drops the file and
    /// stops the sampling until the reopen timer finds the line readable again.
    fn poll_mute_gpio(&mut self, m: &mut Machine) {
        let Some(f) = &self.mute_gpio else { return };
        match gpio_muted(f) {
            Ok(v) => m.set_muted(v, true),
            Err(e) => {
                error!(
                    "input: read gpio{}: {}",
                    self.model.mute_gpio,
                    crate::errstr(&e)
                );
                self.mute_gpio = None;
                m.disarm(Tick::MuteGpio);
            }
        }
    }
}

/// Resolves when `fd` is readable, Err when the reactor cannot poll it (the device is then
/// closed for the reopen timer); never without a device.
async fn readable(fd: Option<&AsyncFd<Evdev>>) -> io::Result<()> {
    match fd {
        Some(fd) => fd.readable().await.map(drop),
        None => std::future::pending().await,
    }
}

async fn run(mut m: Machine, mut devs: Devices, mut alive: UnboundedReceiver<()>) {
    let mut buf = [sys::EMPTY_EVENT; 16];
    loop {
        tokio::select! {
            biased;
            t = m.timers.next() => match t {
                Tick::Reopen => devs.reopen_missing(&mut m),
                Tick::MuteGpio => devs.poll_mute_gpio(&mut m),
                t => m.on_tick(t, |m| {
                    drain(m, &mut devs, Dev::Keys, &mut buf, Read::Now);
                    drain(m, &mut devs, Dev::Mute, &mut buf, Read::Now);
                }),
            },
            r = readable(devs.keys.as_ref()) => ready(&mut m, &mut devs, Dev::Keys, r, &mut buf),
            r = readable(devs.mute.as_ref()) => ready(&mut m, &mut devs, Dev::Mute, r, &mut buf),
            None = alive.recv() => break,
        }
    }
}

fn ready(
    m: &mut Machine,
    devs: &mut Devices,
    dev: Dev,
    r: io::Result<()>,
    buf: &mut [RawInputEvent],
) {
    match r {
        Ok(()) => drain(m, devs, dev, buf, Read::Ready),
        Err(e) => {
            error!("input: poll: {}", crate::errstr(&e));
            devs.close(dev, m);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    /// After the reactor reported the device readable; EAGAIN clears that.
    Ready,
    /// Whatever is queued now, regardless of the reactor (before a due timer).
    Now,
}

/// Reads everything `dev` has (until EAGAIN); a read error closes it for the reopen timer.
fn drain(m: &mut Machine, devs: &mut Devices, dev: Dev, buf: &mut [RawInputEvent], how: Read) {
    let no_dev = || io::Error::from(io::ErrorKind::NotFound);
    loop {
        let Some(fd) = devs.get(dev) else { return };
        let r = match how {
            Read::Ready => fd.try_io(Interest::READABLE, |d| d.read_events(buf)),
            // A readiness flag left set by this read only costs one EAGAIN later.
            Read::Now => fd.get_ref().read_events(buf),
        };
        match r {
            Ok(0) => return,
            Ok(n) => {
                let (keys, mute) = (devs.keys.as_ref(), devs.mute.as_ref());
                m.process(
                    dev,
                    fd.as_raw_fd(),
                    &buf[..n],
                    &mut || keys.map_or_else(|| Err(no_dev()), |k| k.get_ref().key_state()),
                    &mut || mute.map_or_else(|| Err(no_dev()), |k| k.get_ref().switch_state()),
                );
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return;
            }
            Err(e) => {
                error!("input: read: {}", crate::errstr(&e));
                devs.close(dev, m);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
