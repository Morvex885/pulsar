//! Кнопки через evdev: устройства ищутся по имени в /proc/bus/input/devices, имена — из Model.
//! Миди: сенсорные кнопки "awinic_sar", переключатель микрофонов "mute". Лайт 2: сенсорные кнопки
//! "aw9610x" на плате plum и "aw933xx" на плате prune, переключатель "gpio-keys". Мини 3: сенсорные
//! кнопки "aw933xx", переключатель "gpio-keys". Мини 3 Про: сенсорные кнопки "aw933xx",
//! переключатель "mute". Станция 2: сенсорные кнопки "midi_touch", переключатель "gpio-keys".
//! Мини 2: сенсорные кнопки "sar0_pad0", устройства-переключателя нет: фиксируемая линия
//! отключения микрофонов — GPIO (`Model::mute_gpio`, низкий уровень = микрофоны выключены),
//! опрос каждые 100 мс. Мини: клавиатура "adc_keypad" только с кнопкой отключения микрофонов без
//! фиксации. Лайт: сенсорные кнопки "sar0_pad0" или "cy8cmbr3108", кнопка отключения микрофонов
//! без фиксации — отдельное устройство "gpio-keys-filter" (`Model::mute_key_input`, только KEY_MUTE).
//! У остальных коды кнопок и переключателя общие:
//!
//! сенсорные кнопки (ёмкостные, только EV_KEY; KEY bitmap Миди "40 0 0 0 0 0 0 0 c000000000000 0"):
//!   Model::action_key (KEY_VOICECOMMAND 582) -> Action
//!   KEY_VOLUMEUP     (115) -> VolumeUp
//!   KEY_VOLUMEDOWN   (114) -> VolumeDown
//!   Model::mute_key  (KEY_MUTE 113 на adc_keypad Мини) -> MuteToggle (отключение микрофонов
//!   там программное)
//! переключатель (gpio-keys, KEY bitmap бит 113 KEY_MUTE, SW bitmap 0x4000 = SW_MUTE_DEVICE (14)):
//!   аппаратный ползунок или кнопка отключения микрофонов; SW_MUTE_DEVICE=1 — микрофоны
//!   выключены. Его нажатия и отпускания KEY_MUTE игнорируются, источник правды — состояние
//!   переключателя (начальное значение — через EVIOCGSW).
//! Action: Short при отпускании раньше 800 мс, Long сразу, как только кнопку продержали 800 мс.
//! Volume*: Short при нажатии, при удержании дольше 500 мс — автоповтор каждые 200 мс, не дольше
//! 10 с.
//! Четыре физических нажатия mute через evdev открывают окно Bluetooth. Опрос GPIO
//! для жеста не годится: между чтениями могут пропасть переходы.
//! Пропавшие устройства (ENODEV) ищутся заново каждые 3 с; SYN_DROPPED запускает пересинхронизацию
//! состояния (кроме кнопки без фиксации: пропущенное нажатие не восстановить).

use std::cell::Cell;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::FileExt as _;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

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
const KEY_VOLUMEDOWN: u16 = 114;
const KEY_VOLUMEUP: u16 = 115;
const KEY_PLAYPAUSE: u16 = 164;

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
const MUTE_PRESS_MIN: Duration = Duration::from_millis(50);
const MUTE_PRESS_MAX: Duration = Duration::from_millis(500);

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
    /// Изменение блокировки при потере или синхронизации положения переключателя.
    MuteSync(bool),
    /// Momentary mute button that does not latch the microphone itself (the Mini's KEY_MUTE):
    /// the controller toggles its software mute.
    MuteToggle,
    /// Четыре последовательных физических нажатия mute.
    BluetoothPairing,
    /// Нажатие выделенной кнопки воспроизведения, если её отдаёт evdev.
    PlayPause,
}

/// Keeps the input actor alive; the actor stops when the last clone is dropped.
#[derive(Clone)]
pub struct InputHandle {
    muted: Rc<Cell<bool>>,
    available: Rc<Cell<bool>>,
    _alive: UnboundedSender<()>,
}

impl InputHandle {
    /// При неизвестном положении аппаратного переключателя микрофоны заблокированы.
    /// Без аппаратного переключателя всегда false.
    pub fn hw_muted(&self) -> bool {
        self.muted.get()
    }

    pub fn is_available(&self) -> bool {
        self.available.get()
    }
}

/// Открывает сенсорные кнопки (`Model::keys_input`) и источник отключения микрофонов:
/// переключатель (`Model::mute_input` или GPIO `Model::mute_gpio`) либо отдельное устройство
/// кнопки без фиксации (`Model::mute_key_input`). Недоступные устройства ищутся каждые 3 с,
/// даже если при запуске не открылось ни одного.
pub fn spawn_input(model: &'static Model) -> (InputHandle, UnboundedReceiver<InputEvent>) {
    spawn_input_with(model, read_input_devices, Evdev::open)
}

fn spawn_input_with(
    model: &'static Model,
    list: impl Fn() -> String + 'static,
    open: impl Fn(&str, &str) -> io::Result<Evdev> + 'static,
) -> (InputHandle, UnboundedReceiver<InputEvent>) {
    let muted = Rc::new(Cell::new(false));
    let available = Rc::new(Cell::new(false));
    let (tx, rx) = unbounded_channel();
    let mut m = Machine::new(Rc::clone(&muted), tx, model);
    let mut devs = Devices::empty(model, Rc::clone(&available));
    devs.open_missing(&mut m, &list, &open, Evdev::switch_state, true);
    m.arm_repeating(Tick::Reopen, REOPEN_MS, REOPEN_MS);
    let (alive_tx, alive_rx) = unbounded_channel();
    pulsar_core::actors::spawn_actor("input", run(m, devs, alive_rx, list, open));
    (
        InputHandle {
            muted,
            available,
            _alive: alive_tx,
        },
        rx,
    )
}

/// Устройство в слоте «mute»: переключатель микрофонов (`Model::mute_input`) или, если его нет,
/// отдельное устройство кнопки без фиксации (`Model::mute_key_input`); пусто — ни того, ни другого.
fn mute_dev_name(model: &Model) -> &'static str {
    if model.mute_input.is_empty() {
        model.mute_key_input
    } else {
        model.mute_input
    }
}

/// Устройство в слоте «mute» — переключатель с EV_SW (иначе — кнопка без фиксации или ничего).
fn mute_dev_is_switch(model: &Model) -> bool {
    !model.mute_input.is_empty()
}

/// Имя и обработчик должны быть в одном блоке /proc/bus/input/devices.
fn event_handlers(devices: &str) -> impl Iterator<Item = (&str, &str)> {
    devices.split("\n\n").filter_map(|block| {
        let name = block
            .lines()
            .find_map(|line| line.strip_prefix("N: Name=\"")?.strip_suffix('"'))?;
        let handler = block
            .lines()
            .find_map(|line| line.strip_prefix("H: Handlers="))?
            .split_whitespace()
            .find(|t| {
                t.strip_prefix("event")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })?;
        Some((name, handler))
    })
}

fn try_named<T>(
    devices: &str,
    names: &[&str],
    mut open: impl FnMut(&str, &str) -> Option<T>,
) -> Option<T> {
    names.iter().filter(|n| !n.is_empty()).find_map(|&name| {
        event_handlers(devices)
            .filter(|(n, _)| *n == name)
            .find_map(|(_, handler)| open(name, handler))
    })
}

fn read_input_devices() -> String {
    pulsar_core::sysfs::read_file("/proc/bus/input/devices").unwrap_or_default()
}

fn open_dev(
    devices: &str,
    names: &[&str],
    quiet: bool,
    open: impl Fn(&str, &str) -> io::Result<Evdev>,
) -> Option<AsyncFd<Evdev>> {
    let fd = try_named(devices, names, |name, handler| {
        let path = format!("/dev/input/{handler}");
        match open(&path, name) {
            Ok(dev) => register_dev(dev, name, &path),
            Err(e) => {
                if !quiet {
                    error!("input: open {path}: {}", crate::errstr(&e));
                }
                None
            }
        }
    });
    if fd.is_none() && !quiet && names.iter().any(|n| !n.is_empty()) {
        error!("input: no usable device among {names:?}");
    }
    fd
}

fn register_dev(dev: Evdev, name: &str, path: &str) -> Option<AsyncFd<Evdev>> {
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
fn open_mute_gpio(dir: &str, gpio: u32, quiet: bool) -> Option<(File, bool)> {
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
    let opened = File::open(&path).and_then(|f| gpio_muted(&f).map(|muted| (f, muted)));
    match opened {
        Ok(value) => {
            info!("input: mute switch on gpio{gpio}");
            Some(value)
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
    match b[0] {
        b'0' => Ok(true),
        b'1' => Ok(false),
        _ => Err(io::ErrorKind::InvalidData.into()),
    }
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
    /// KEY_VOICECOMMAND of this model; 0: no action button.
    action_key: u16,
    /// Momentary mic mute button of this model (`Model::mute_key`); 0: none.
    mute_key: u16,
    mute_key_dev: Dev,
    /// Устройство в слоте «mute» — переключатель (EV_SW, состояние через EVIOCGSW); false —
    /// кнопка без фиксации (`Model::mute_key_input`), у неё только нажатия `mute_key`.
    mute_is_switch: bool,
    /// Неизвестно до чтения положения и после потери аппаратного переключателя.
    mute_position: Option<bool>,
    mute_series: Option<(Duration, u8)>,
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
    fn new(muted: Rc<Cell<bool>>, tx: UnboundedSender<InputEvent>, model: &Model) -> Self {
        muted.set(mute_dev_is_switch(model) || model.mute_gpio != 0);
        Self {
            timers: Timers::new(),
            epoch: Instant::now(),
            muted,
            tx,
            action_key: model.action_key,
            mute_key: model.mute_key,
            mute_key_dev: if model.mute_key_input.is_empty() {
                Dev::Keys
            } else {
                Dev::Mute
            },
            mute_is_switch: mute_dev_is_switch(model),
            mute_position: None,
            mute_series: None,
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
        if self.action_key != 0 && code == self.action_key {
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

    fn set_mute_position(&mut self, position: Option<bool>, notify: bool) {
        let previous = std::mem::replace(&mut self.mute_position, position);
        let muted = position.unwrap_or(true);
        if self.muted.replace(muted) == muted {
            return;
        }
        info!("input: hardware mute {}", if muted { "on" } else { "off" });
        if notify {
            let event = if previous.is_some() && position.is_some() {
                InputEvent::Mute(muted)
            } else {
                InputEvent::MuteSync(muted)
            };
            let _ = self.tx.send(event);
        }
    }

    fn mute_press(&mut self, e: &RawInputEvent) {
        // Используем время ядра, а не время чтения: несколько нажатий могут прийти одним пакетом.
        let now = Duration::new(e.sec as _, e.usec as u32 * 1000);
        let count = match self.mute_series {
            Some((last, count)) => match now.checked_sub(last) {
                Some(gap) if gap < MUTE_PRESS_MIN => return,
                Some(gap) if gap <= MUTE_PRESS_MAX => count + 1,
                _ => 1,
            },
            None => 1,
        };
        self.mute_series = if count == 4 {
            let _ = self.tx.send(InputEvent::BluetoothPairing);
            None
        } else {
            Some((now, count))
        };
    }

    fn read_mute_state(&mut self, bits: io::Result<SwBits>, notify: bool) -> io::Result<()> {
        self.mute_series = None;
        let bits = bits.inspect_err(|e| warn!("input: EVIOCGSW: {}", crate::errstr(e)))?;
        self.set_mute_position(Some(test_bit(&bits, SW_MUTE_DEVICE)), notify);
        Ok(())
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
        if self.action_key != 0 && self.action_down.is_some() && !test_bit(&b, self.action_key) {
            self.on_key(self.action_key, 0);
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
    ) -> io::Result<()> {
        for e in events {
            let dropped = match dev {
                Dev::Keys => self.keys_dropped,
                Dev::Mute => self.mute_dropped,
            };
            if e.type_ == EV_SYN {
                if e.code == SYN_DROPPED {
                    warn!("input: events dropped on fd {fd}, resyncing");
                    self.mute_series = None;
                    self.set_dropped(dev, true);
                    if dev == Dev::Mute && self.mute_is_switch {
                        self.set_mute_position(None, true);
                    }
                } else if e.code == SYN_REPORT && dropped {
                    self.set_dropped(dev, false);
                    match dev {
                        Dev::Keys => self.resync_keys(key_state()),
                        Dev::Mute if self.mute_is_switch => {
                            // Снимок EVIOCGSW новее всех событий из уже прочитанного буфера.
                            return self.read_mute_state(switch_state(), true);
                        }
                        // Кнопка без фиксации: пропущенное нажатие не восстановить.
                        Dev::Mute => {}
                    }
                }
                continue;
            }
            if dropped {
                continue;
            }
            match e.type_ {
                EV_KEY if self.mute_key != 0 && e.code == self.mute_key => {
                    // Считается только нажатие: 2 — автоповтор, 0 — отпускание.
                    if dev == self.mute_key_dev && e.value == 1 {
                        info!("input: mute button");
                        let _ = self.tx.send(InputEvent::MuteToggle);
                        self.mute_press(e);
                    }
                }
                EV_KEY if dev == Dev::Keys && e.code == KEY_PLAYPAUSE => {
                    if e.value == 1 {
                        let _ = self.tx.send(InputEvent::PlayPause);
                    }
                }
                EV_KEY if dev == Dev::Keys => self.on_key(e.code, e.value),
                EV_SW if dev == Dev::Mute && self.mute_is_switch && e.code == SW_MUTE_DEVICE => {
                    let muted = e.value != 0;
                    let press = matches!(e.value, 0 | 1) && self.mute_position == Some(!muted);
                    self.set_mute_position(Some(muted), true);
                    if press {
                        self.mute_press(e);
                    }
                }
                _ => {}
            }
        }
        Ok(())
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
    available: Rc<Cell<bool>>,
    keys: Option<AsyncFd<Evdev>>,
    mute: Option<AsyncFd<Evdev>>,
    /// Value file of `Model::mute_gpio`.
    mute_gpio: Option<File>,
}

impl Devices {
    fn empty(model: &'static Model, available: Rc<Cell<bool>>) -> Self {
        Self {
            model,
            available,
            keys: None,
            mute: None,
            mute_gpio: None,
        }
    }

    fn update_availability(&self) {
        self.available
            .set(self.keys.is_some() || self.mute.is_some() || self.mute_gpio.is_some());
    }

    fn get(&self, dev: Dev) -> Option<&AsyncFd<Evdev>> {
        match dev {
            Dev::Keys => self.keys.as_ref(),
            Dev::Mute => self.mute.as_ref(),
        }
    }

    /// Drops a device for the reopen timer; keys still held will never be released.
    fn close(&mut self, dev: Dev, m: &mut Machine) {
        m.mute_series = None;
        match dev {
            Dev::Keys => {
                m.release_all();
                self.keys = None;
            }
            Dev::Mute => {
                self.mute = None;
                if m.mute_is_switch {
                    m.set_mute_position(None, true);
                }
            }
        }
        self.update_availability();
    }

    fn open_missing(
        &mut self,
        m: &mut Machine,
        list: impl Fn() -> String,
        open: impl Fn(&str, &str) -> io::Result<Evdev>,
        switch_state: impl FnOnce(&Evdev) -> io::Result<SwBits>,
        initial: bool,
    ) {
        let mute_name = mute_dev_name(self.model);
        let need_keys = self.keys.is_none() && !self.model.keys_input.is_empty();
        let need_mute = self.mute.is_none() && !mute_name.is_empty();
        if need_keys || need_mute {
            let devices = list();
            if need_keys {
                self.keys = open_dev(&devices, self.model.keys_input, !initial, &open);
                m.keys_dropped = false;
            }
            if need_mute {
                self.mute = open_dev(&devices, &[mute_name], !initial, &open);
            }
        }
        if need_mute {
            m.mute_dropped = false;
            if m.mute_is_switch
                && let Some(fd) = &self.mute
                && m.read_mute_state(switch_state(fd.get_ref()), !initial)
                    .is_err()
            {
                self.close(Dev::Mute, m);
            }
        }
        self.open_gpio(m, GPIO_DIR, initial);
        self.update_availability();
    }

    fn open_gpio(&mut self, m: &mut Machine, dir: &str, initial: bool) {
        if self.mute_gpio.is_none()
            && self.model.mute_gpio != 0
            && let Some((file, muted)) = open_mute_gpio(dir, self.model.mute_gpio, !initial)
        {
            self.mute_gpio = Some(file);
            m.set_mute_position(Some(muted), !initial);
            m.arm_repeating(Tick::MuteGpio, GPIO_POLL_MS, GPIO_POLL_MS);
        }
    }

    /// Samples the mute line (sysfs value files are always readable: waiting for POLLPRI would
    /// need an `edge` written to the pin, which stays as found). A read error drops the file and
    /// stops the sampling until the reopen timer finds the line readable again.
    fn poll_mute_gpio(&mut self, m: &mut Machine) {
        let Some(f) = &self.mute_gpio else { return };
        match gpio_muted(f) {
            Ok(v) => m.set_mute_position(Some(v), true),
            Err(e) => {
                error!(
                    "input: read gpio{}: {}",
                    self.model.mute_gpio,
                    crate::errstr(&e)
                );
                self.mute_gpio = None;
                m.mute_series = None;
                m.set_mute_position(None, true);
                self.update_availability();
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

async fn run(
    mut m: Machine,
    mut devs: Devices,
    mut alive: UnboundedReceiver<()>,
    list: impl Fn() -> String,
    open: impl Fn(&str, &str) -> io::Result<Evdev>,
) {
    let mut buf = [sys::EMPTY_EVENT; 16];
    loop {
        tokio::select! {
            biased;
            t = m.timers.next() => match t {
                Tick::Reopen => devs.open_missing(&mut m, &list, &open, Evdev::switch_state, false),
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

/// Читает до EAGAIN. При EOF или ошибке закрывает устройство до следующего поиска.
fn drain(m: &mut Machine, devs: &mut Devices, dev: Dev, buf: &mut [RawInputEvent], how: Read) {
    loop {
        let Some(fd) = devs.get(dev) else { return };
        let r = match how {
            Read::Ready => fd.try_io(Interest::READABLE, |d| d.read_events(buf)),
            // A readiness flag left set by this read only costs one EAGAIN later.
            Read::Now => fd.get_ref().read_events(buf),
        };
        match r {
            Ok(0) => {
                devs.close(dev, m);
                return;
            }
            Ok(n) => {
                if m.process(
                    dev,
                    fd.as_raw_fd(),
                    &buf[..n],
                    &mut || fd.get_ref().key_state(),
                    &mut || fd.get_ref().switch_state(),
                )
                .is_err()
                {
                    devs.close(dev, m);
                    return;
                }
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
