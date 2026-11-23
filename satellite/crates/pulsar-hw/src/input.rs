//! Кнопки через evdev: устройства ищутся по имени в /proc/bus/input/devices, имена — из Model.
//! Миди: сенсорные кнопки "awinic_sar", переключатель микрофонов "mute". Лайт 2: сенсорные кнопки
//! "aw9610x" на плате plum и "aw933xx" на плате prune, переключатель "gpio-keys". Мини 3: сенсорные
//! кнопки "aw933xx", переключатель "gpio-keys". Мини 3 Про: сенсорные кнопки "aw933xx",
//! переключатель "mute". Станция 2: сенсорные кнопки "midi_touch", переключатель "gpio-keys".
//! Мини 2: сенсорные кнопки "sar0_pad0", устройства-переключателя нет: фиксируемая линия
//! отключения микрофонов — GPIO (`Model::mute_gpio`, низкий уровень = микрофоны выключены),
//! опрос каждые 100 мс. Мини: клавиатура "adc_keypad" только с кнопкой отключения микрофонов без
//! фиксации. Лайт: сенсорные кнопки "sar0_pad0", кнопка отключения микрофонов без фиксации —
//! отдельное устройство "gpio-keys-filter" (`Model::mute_key_input`, только KEY_MUTE, без EV_SW).
//! Без устройства с именем из Model сенсорные кнопки ищутся по кодам (`pick_keys_dev`).
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
//! Пропавшие устройства (ENODEV) ищутся заново каждые 3 с; SYN_DROPPED запускает пересинхронизацию
//! состояния (кроме кнопки без фиксации: пропущенное нажатие не восстановить).

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
    /// Momentary mute button that does not latch the microphone itself (the Mini's KEY_MUTE):
    /// the controller toggles its software mute.
    MuteToggle,
}

/// Keeps the input actor alive; the actor stops when the last clone is dropped.
#[derive(Clone)]
pub struct InputHandle {
    muted: Rc<Cell<bool>>,
    _alive: UnboundedSender<()>,
}

impl InputHandle {
    /// Состояние переключателя микрофонов: читается при старте (без события `Mute`), затем актор
    /// обновляет его перед каждым `Mute` и при переоткрытии. Без переключателя (Мини, Лайт —
    /// кнопка без фиксации) всегда false.
    pub fn hw_muted(&self) -> bool {
        self.muted.get()
    }
}

/// Открывает сенсорные кнопки (`Model::keys_input`) и источник отключения микрофонов:
/// переключатель (`Model::mute_input` или GPIO `Model::mute_gpio`) либо отдельное устройство
/// кнопки без фиксации (`Model::mute_key_input`); Err, если не открылось ничего. Пропавшие
/// устройства ищутся заново каждые 3 с.
pub fn spawn_input(
    model: &'static Model,
) -> Result<(InputHandle, UnboundedReceiver<InputEvent>), String> {
    let mut keys_miss = String::new();
    let keys = open_keys(model, false, &mut keys_miss);
    let mute_name = mute_dev_name(model);
    let mute = (!mute_name.is_empty())
        .then(|| open_dev(mute_name, false))
        .flatten();
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
    let mut m = Machine::new(Rc::clone(&muted), tx, model.action_key, model.mute_key);
    m.mute_is_switch = mute_dev_is_switch(model);
    // The state the controller starts from: no event for it.
    if m.mute_is_switch
        && let Some(fd) = &mute
    {
        m.read_mute_state(fd.get_ref().switch_state(), false);
    }
    if let Some(f) = &mute_gpio
        && let Ok(v) = gpio_muted(f)
    {
        m.set_muted(v, false);
    }
    if (m.mute_is_switch && mute.is_some()) || mute_gpio.is_some() {
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
        keys_miss,
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

/// Устройство ввода из /proc/bus/input/devices.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InputDev {
    name: String,
    /// /dev/input/eventN.
    path: String,
    /// `I: Bus=`, BUS_* из linux/input.h.
    bus: u16,
}

// Шины подключаемых и программных устройств: USB- и BT-клавиатуры, пульты и uinput тоже бывают с
// KEY_VOICECOMMAND и громкостью, но кнопками станции не бывают.
const BUS_USB: u16 = 0x03;
const BUS_BLUETOOTH: u16 = 0x05;
const BUS_VIRTUAL: u16 = 0x06;

/// Устройства с обработчиком event в листинге /proc/bus/input/devices, в порядке листинга.
fn event_devs_in(devices: &str) -> Vec<InputDev> {
    let mut out = Vec::new();
    let (mut name, mut bus) = (None, 0);
    for line in devices.split('\n') {
        if let Some(i) = line.strip_prefix("I: Bus=") {
            let hex = i.split_whitespace().next().unwrap_or_default();
            bus = u16::from_str_radix(hex, 16).unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("N: Name=") {
            name = n.strip_prefix('"').and_then(|n| n.strip_suffix('"'));
        } else if let Some(handlers) = line.strip_prefix("H: Handlers=") {
            if let Some(n) = name.take()
                && let Some(ev) = handlers.split_whitespace().find(|t| t.starts_with("event"))
            {
                out.push(InputDev {
                    name: n.to_owned(),
                    path: format!("/dev/input/{ev}"),
                    bus,
                });
            }
        } else if line.is_empty() {
            (name, bus) = (None, 0);
        }
    }
    out
}

/// /dev/input/eventN of the device with the given name in a /proc/bus/input/devices listing.
fn find_event_dev_in(devices: &str, name: &str) -> Option<String> {
    event_devs_in(devices)
        .into_iter()
        .find(|d| d.name == name)
        .map(|d| d.path)
}

/// Устройство сенсорных кнопок: `Model::keys_input` по имени, а если его нет — первое, у которого
/// есть все коды кнопок (`has_keys`). Не берутся устройство слота «mute» (его читает свой слот) и
/// подключаемые или программные; см. docs/lite.md, «Кнопки и микрофоны».
fn pick_keys_dev<'a>(
    devs: &'a [InputDev],
    model: &Model,
    mut has_keys: impl FnMut(&InputDev) -> bool,
) -> Option<&'a InputDev> {
    if model.keys_input.is_empty() {
        return None;
    }
    if let Some(d) = devs.iter().find(|d| d.name == model.keys_input) {
        return Some(d);
    }
    if model.action_key == 0 {
        return None;
    }
    let mute_name = mute_dev_name(model);
    devs.iter().find(|d| {
        (mute_name.is_empty() || d.name != mute_name)
            && !matches!(d.bus, BUS_USB | BUS_BLUETOOTH | BUS_VIRTUAL)
            && has_keys(d)
    })
}

fn read_input_devices() -> String {
    pulsar_core::sysfs::read_file("/proc/bus/input/devices").unwrap_or_default()
}

fn open_dev(name: &str, quiet: bool) -> Option<AsyncFd<Evdev>> {
    let Some(path) = find_event_dev_in(&read_input_devices(), name) else {
        if !quiet {
            error!("input: device \"{name}\" not found");
        }
        return None;
    };
    open_evdev(&path, quiet).and_then(|dev| register_dev(dev, name, &path))
}

/// Открывает устройство `pick_keys_dev`. `miss` — листинг /proc/bus/input/devices прошлого поиска,
/// не нашедшего устройства: пока листинг тот же, перебор не повторяется (переоткрытие идёт каждые
/// `REOPEN_MS`, а первый open и последний close evdev будят драйвер устройства). Если проверка
/// кандидата не удалась (open или EVIOCGBIT вернули ошибку), листинг не запоминается: ответ «кодов
/// нет» недостоверен, и следующий тик таймера переберёт устройства снова.
fn open_keys(model: &Model, quiet: bool, miss: &mut String) -> Option<AsyncFd<Evdev>> {
    if model.keys_input.is_empty() {
        return None;
    }
    let devices = read_input_devices();
    if !miss.is_empty() && devices == *miss {
        return None;
    }
    let devs = event_devs_in(&devices);
    let codes = [model.action_key, KEY_VOLUMEUP, KEY_VOLUMEDOWN];
    let probe_failed = Cell::new(false);
    let found = pick_keys_dev(&devs, model, |d| {
        match Evdev::open(&d.path).and_then(|dev| dev.key_caps()) {
            Ok(caps) => codes.iter().all(|&c| test_bit(&caps, c)),
            Err(e) => {
                debug!("input: probe {}: {}", d.path, crate::errstr(&e));
                probe_failed.set(true);
                false
            }
        }
    });
    let Some(d) = found else {
        if !quiet {
            error!("input: device \"{}\" not found", model.keys_input);
        }
        if probe_failed.get() {
            miss.clear();
        } else {
            *miss = devices;
        }
        return None;
    };
    miss.clear();
    if d.name != model.keys_input {
        info!(
            "input: no \"{}\", buttons found by their key codes",
            model.keys_input
        );
    }
    open_evdev(&d.path, quiet).and_then(|dev| register_dev(dev, &d.name, &d.path))
}

fn open_evdev(path: &str, quiet: bool) -> Option<Evdev> {
    match Evdev::open(path) {
        Ok(d) => Some(d),
        Err(e) => {
            if !quiet {
                error!("input: open {path}: {}", crate::errstr(&e));
            }
            None
        }
    }
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
    /// KEY_VOICECOMMAND of this model; 0: no action button.
    action_key: u16,
    /// Momentary mic mute button of this model (`Model::mute_key`); 0: none.
    mute_key: u16,
    /// Устройство в слоте «mute» — переключатель (EV_SW, состояние через EVIOCGSW); false —
    /// кнопка без фиксации (`Model::mute_key_input`), у неё только нажатия `mute_key`.
    mute_is_switch: bool,
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
    fn new(
        muted: Rc<Cell<bool>>,
        tx: UnboundedSender<InputEvent>,
        action_key: u16,
        mute_key: u16,
    ) -> Self {
        Self {
            timers: Timers::new(),
            epoch: Instant::now(),
            muted,
            tx,
            action_key,
            mute_key,
            mute_is_switch: true,
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
        if self.mute_key != 0 && code == self.mute_key {
            // value 1 is the press; 2 is autorepeat and 0 the release.
            if value == 1 {
                info!("input: mute button");
                let _ = self.tx.send(InputEvent::MuteToggle);
            }
            return;
        }
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
                        Dev::Mute if self.mute_is_switch => {
                            self.read_mute_state(switch_state(), true);
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
            if e.type_ == EV_KEY && dev == Dev::Keys {
                self.on_key(e.code, e.value);
            } else if e.type_ == EV_KEY
                && dev == Dev::Mute
                && !self.mute_is_switch
                && self.mute_key != 0
                && e.code == self.mute_key
            {
                // KEY_MUTE переключателя игнорируется (источник правды — EV_SW), а у отдельной
                // кнопки без фиксации это и есть нажатие.
                self.on_key(e.code, e.value);
            } else if e.type_ == EV_SW && e.code == SW_MUTE_DEVICE && self.mute_is_switch {
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
    /// Листинг устройств ввода, в котором `open_keys` кнопок не нашёл.
    keys_miss: String,
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
            self.keys = open_keys(self.model, true, &mut self.keys_miss);
            m.keys_dropped = false;
        }
        let mute_name = mute_dev_name(self.model);
        if self.mute.is_none() && !mute_name.is_empty() {
            self.mute = open_dev(mute_name, true);
            m.mute_dropped = false;
            if m.mute_is_switch
                && let Some(fd) = &self.mute
            {
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
