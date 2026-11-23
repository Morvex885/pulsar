//! Актор режима: опрос BlueZ через `dbus-send`, включение/выключение видимости, AVRCP-команды
//! подключённому источнику. См. docs/bluetooth.md, «Обнаружение и канал управления».

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use log::{info, warn};
use pulsar_core::{Timers, actors};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::BtAction;

/// Интервал опроса: телефон может подключиться сам, и состояние адаптера меняется не только от
/// наших команд; один `dbus-send` — десятки миллисекунд CPU.
const POLL_MS: u64 = 2_000;
/// Первый опрос — через полсекунды после старта актора: bluetoothd к этому моменту уже
/// принимает соединения, а handle ещё ничего не отдаёт контроллеру.
const FIRST_POLL_MS: u64 = 500;
/// Вызов к зависшему bluetoothd не должен держать очередь актора.
const CALL_TIMEOUT_MS: u64 = 1_500;
/// Абсолютный путь: PATH демона на станции может быть узким.
const DBUS_SEND: &str = "/usr/bin/dbus-send";
/// Один адаптер на станцию: тот же hci0, который ищет `adapter_present` (lib.rs).
const ADAPTER: &str = "/org/bluez/hci0";
/// Объект AVRCP-пульта подключённого источника (doc/media-api.rst BlueZ).
const PLAYER_SUFFIX: &str = "/player0";

pub enum BtEvent {
    /// `status()` / `discoverable()` могли измениться.
    Changed,
}

enum Cmd {
    SetMode(bool),
    Avr(BtAction),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tk {
    Poll,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Device {
    pub path: String,
    pub name: String,
    pub connected: bool,
    /// org.bluez.MediaPlayer1.Status: "", "playing", "paused", "stopped", …
    pub status: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Snap {
    pub discoverable: bool,
    pub devices: Vec<Device>,
}

/// Устройство по path из снимка; создаётся при первом встреченном свойстве.
fn entry<'a>(snap: &'a mut Snap, path: &str) -> &'a mut Device {
    match snap.devices.iter().position(|d| d.path == path) {
        Some(i) => &mut snap.devices[i],
        None => {
            snap.devices.push(Device {
                path: path.to_owned(),
                ..Default::default()
            });
            snap.devices.last_mut().unwrap()
        }
    }
}

/// Текст dbus-send по ObjectManager.GetManagedObjects org.bluez → снимок. Формат вывода
/// (замер на станции, 2026-11-23): путь объекта — `object path "…"`, ключи интерфейсов и
/// свойств dbus-printer печатает строками `string "<имя>"`, значения — `variant <тип>
/// <значение>`; вложенные массивы/словари несут свои `string`-строки, которые не должны
/// попадать в свойства (см. тест на реальном дампе).
pub(crate) fn parse_managed(text: &str) -> Snap {
    #[derive(Default)]
    struct Ctx {
        obj: String,
        iface: u8, // 1 Adapter1, 2 Device1, 3 MediaPlayer1
        prop: u8,  // 1 Discoverable, 2 Connected, 3 Name, 4 Status
    }
    let mut snap = Snap::default();
    let mut players: Vec<(String, String)> = Vec::new();
    let mut c = Ctx::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("object path \"") {
            c.obj = rest.trim_end_matches('"').to_owned();
            c.iface = 0;
            c.prop = 0;
            continue;
        }
        // Ключ словаря приходит как `string "<имя>"`; голая кавычка — совместимость с
        // другим выводом dbus-send. Значения (`variant …`) префикс не имеют.
        let l = l.strip_prefix("string ").unwrap_or(l);
        // Интерфейс вне списка сбрасывает iface: его свойства (например Battery1 «Name») не
        // должны приписаться Device1.
        if l.starts_with("\"org.") {
            c.iface = match l {
                "\"org.bluez.Adapter1\"" => 1,
                "\"org.bluez.Device1\"" => 2,
                "\"org.bluez.MediaPlayer1\"" => 3,
                _ => 0,
            };
            continue;
        }
        match l {
            "\"Discoverable\"" => {
                c.prop = if c.iface == 1 { 1 } else { 0 };
                continue;
            }
            "\"Connected\"" => {
                c.prop = if c.iface == 2 { 2 } else { 0 };
                continue;
            }
            "\"Name\"" => {
                c.prop = if c.iface == 2 { 3 } else { 0 };
                continue;
            }
            "\"Status\"" => {
                c.prop = if c.iface == 3 { 4 } else { 0 };
                continue;
            }
            _ => {}
        }
        // Значение свойства: "variant <тип> <значение>"; variant array/dict нам не нужны.
        let Some(v) = l.strip_prefix("variant").map(str::trim) else {
            continue;
        };
        let p = std::mem::take(&mut c.prop);
        if p == 0 {
            continue;
        }
        let truthy = v == "boolean true";
        let s = match v.strip_prefix("string \"") {
            // ровно одна ведущая и одна хвостовая кавычка: имя устройства может содержать \"
            Some(q) => q.strip_suffix('"').unwrap_or(q).to_owned(),
            None => String::new(),
        };
        match p {
            1 => snap.discoverable = truthy,
            2 => entry(&mut snap, &c.obj).connected = truthy,
            3 => entry(&mut snap, &c.obj).name = s,
            _ => {
                if let Some(dev) = c.obj.strip_suffix(PLAYER_SUFFIX) {
                    players.push((dev.to_owned(), s));
                }
            }
        }
    }
    for (path, status) in players {
        // player есть только у устройства, известное BlueZ оно или нет — запись создаётся
        // на месте, молчаливой потери AVRCP-статуса не будет.
        entry(&mut snap, &path).status = status;
    }
    snap
}

/// Одна строка статуса: «off», «discoverable», «connected: Phone (playing)», «discoverable,
/// connected: Phone». По префиксу «connected:» потребитель понимает, что играет внешний
/// источник.
pub(crate) fn status_line(snap: &Snap) -> String {
    let mut parts: Vec<String> = Vec::new();
    if snap.discoverable {
        parts.push("discoverable".to_owned());
    }
    if let Some(d) = snap.devices.iter().find(|d| d.connected) {
        let name = if d.name.is_empty() { "device" } else { &d.name };
        let mut p = format!("connected: {name}");
        if !d.status.is_empty() {
            p.push_str(&format!(" ({})", d.status));
        }
        parts.push(p);
    }
    if parts.is_empty() {
        "off".to_owned()
    } else {
        parts.join(", ")
    }
}

struct Shared {
    status: RefCell<String>,
    discoverable: Cell<bool>,
}

#[derive(Clone)]
pub struct BtHandle {
    shared: Rc<Shared>,
    cmds: UnboundedSender<Cmd>,
}

impl BtHandle {
    /// Одна строка для Home Assistant и других подписчиков на статус; до первого опроса пуста.
    pub fn status(&self) -> String {
        self.shared.status.borrow().clone()
    }

    /// Видима ли колонка для сопряжения (свойство org.bluez.Adapter1.Discoverable).
    pub fn discoverable(&self) -> bool {
        self.shared.discoverable.get()
    }

    /// Включить/выключить режим колонки; выключение из включённого режима отключает и
    /// подключённые источники. Фактическое состояние приходит с `BtEvent::Changed`, не здесь.
    pub fn set_discoverable(&self, on: bool) {
        let _ = self.cmds.send(Cmd::SetMode(on));
    }

    /// AVRCP-команда подключённому источнику; без подключённого — команда теряется (лог).
    pub fn avr(&self, action: BtAction) {
        let _ = self.cmds.send(Cmd::Avr(action));
    }
}

/// Вызов `dbus-send`; тесты подменяют заготовленными ответами.
pub(crate) type CallFn =
    Box<dyn Fn(Vec<String>) -> Pin<Box<dyn Future<Output = Result<String, String>>>>>;

/// Запускает опрос адаптера (вызывающий решил, что адаптер есть: `adapter_present`).
pub fn spawn() -> (BtHandle, UnboundedReceiver<BtEvent>) {
    spawn_with(Box::new(dbus_send))
}

pub(crate) fn spawn_with(call: CallFn) -> (BtHandle, UnboundedReceiver<BtEvent>) {
    let (ev_tx, ev_rx) = unbounded_channel();
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let shared = Rc::new(Shared {
        status: RefCell::new(String::new()),
        discoverable: Cell::new(false),
    });
    let svc = Service {
        shared: shared.clone(),
        events: ev_tx,
        call,
        snap: Snap::default(),
        timers: Timers::new(),
    };
    actors::spawn_actor("bluetooth", svc.run(cmd_rx));
    (
        BtHandle {
            shared,
            cmds: cmd_tx,
        },
        ev_rx,
    )
}

async fn dbus_send_inner(args: Vec<String>) -> Result<String, String> {
    let out = tokio::time::timeout(
        Duration::from_millis(CALL_TIMEOUT_MS),
        tokio::process::Command::new(DBUS_SEND)
            .args(&args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "timeout".to_owned())?
    .map_err(|e| format!("spawn dbus-send: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim_start().starts_with("method return") {
        return Ok(text);
    }
    // Ошибочный reply dbus-send печатает строкой "error: …" (коды выхода не стабильны).
    if let Some(err) = text.lines().find(|l| l.trim_start().starts_with("error")) {
        return Err(err.trim().to_owned());
    }
    Err(format!(
        "exit {}, no reply",
        out.status.code().unwrap_or(-1)
    ))
}

fn dbus_send(args: Vec<String>) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
    Box::pin(dbus_send_inner(args))
}

fn managed_args() -> Vec<String> {
    [
        "--system",
        "--print-reply",
        "--dest=org.bluez",
        "/",
        "org.freedesktop.DBus.ObjectManager.GetManagedObjects",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn discoverable_args(on: bool) -> Vec<String> {
    [
        "--system",
        "--print-reply",
        "--dest=org.bluez",
        ADAPTER,
        "org.freedesktop.DBus.Properties.Set",
        "string:org.bluez.Adapter1",
        "string:Discoverable",
        if on {
            "variant:boolean:true"
        } else {
            "variant:boolean:false"
        },
    ]
    .map(str::to_owned)
    .to_vec()
}

fn disconnect_args(path: &str) -> Vec<String> {
    [
        "--system",
        "--print-reply",
        "--dest=org.bluez",
        path,
        "org.bluez.Device1.Disconnect",
    ]
    .map(str::to_owned)
    .to_vec()
}

fn player_args(path: &str, method: &str) -> Vec<String> {
    [
        "--system",
        "--print-reply",
        "--dest=org.bluez",
        path,
        &format!("org.bluez.MediaPlayer1.{method}"),
    ]
    .map(str::to_owned)
    .to_vec()
}

struct Service {
    shared: Rc<Shared>,
    events: UnboundedSender<BtEvent>,
    call: CallFn,
    snap: Snap,
    timers: Timers<Tk>,
}

impl Service {
    async fn run(mut self, mut cmds: UnboundedReceiver<Cmd>) {
        self.timers.set_repeating(Tk::Poll, FIRST_POLL_MS, POLL_MS);
        loop {
            // Таймер первой ветвью biased: контракт pulsar_core::Timers (п. 3).
            tokio::select! {
                biased;
                _k = self.timers.next() => self.poll().await,
                c = cmds.recv() => match c {
                    Some(Cmd::SetMode(on)) => self.set_mode(on).await,
                    Some(Cmd::Avr(a)) => self.avr(a).await,
                    None => break, // все handle-ы брошены (docs/architecture.md, п. 5)
                },
            }
        }
    }

    async fn poll(&mut self) {
        match (self.call)(managed_args()).await {
            Ok(text) => {
                // Живой, но пустой/непонятный дамп (`array [ ]` на перезапуске bluetoothd) —
                // не «off»: ошибка, пока адаптер не вернётся в объектах dbus.
                if !text.contains(ADAPTER) {
                    self.publish_status("error: adapter missing from dbus dump".to_owned());
                    return;
                }
                self.snap = parse_managed(&text);
                let status = status_line(&self.snap);
                self.publish_status(status);
            }
            // Мёртвые bluetoothd/dbus видны в статусе; опрос продолжится по таймеру.
            Err(e) => self.publish_status(format!("error: {e}")),
        }
    }

    async fn set_mode(&mut self, on: bool) {
        if let Err(e) = (self.call)(discoverable_args(on)).await {
            warn!("bluetooth: Discoverable={on}: {e}");
        }
        // Выход из режима рвёт и A2DP-поток: «выключи блютуз» = перестать быть
        // колонкой, а не только перестать быть видимой. Только когда режим был
        // включён: повторный off не должен дёргать источники, подключённые в
        // штатном режиме (у стека ещё и a2dp-source).
        let was_on = self.snap.discoverable;
        if !on && was_on {
            let paths: Vec<String> = self
                .snap
                .devices
                .iter()
                .filter(|d| d.connected)
                .map(|d| d.path.clone())
                .collect();
            for p in paths {
                if let Err(e) = (self.call)(disconnect_args(&p)).await {
                    info!("bluetooth: {p}: {e}"); // источник мог отключиться сам
                }
            }
        }
        self.poll().await;
    }

    async fn avr(&mut self, action: BtAction) {
        let Some(dev) = self.snap.devices.iter().find(|d| d.connected) else {
            info!(
                "bluetooth: '{}' without a connected source",
                action.method()
            );
            return;
        };
        let path = format!("{}{PLAYER_SUFFIX}", dev.path);
        if let Err(e) = (self.call)(player_args(&path, action.method())).await {
            warn!("bluetooth: {} on '{}': {e}", action.method(), dev.name);
        }
        self.poll().await;
    }

    fn publish_status(&mut self, status: String) {
        let changed = *self.shared.status.borrow() != status
            || self.shared.discoverable.get() != self.snap.discoverable;
        if !changed {
            return;
        }
        *self.shared.status.borrow_mut() = status;
        self.shared.discoverable.set(self.snap.discoverable);
        let _ = self.events.send(BtEvent::Changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Bus {
        /// Ответы GetManagedObjects по очереди; после конца очереди — последний извлечённый.
        answers: RefCell<VecDeque<String>>,
        last: RefCell<Option<String>>,
        calls: RefCell<Vec<Vec<String>>>,
    }

    // Дампы — реальный формат вывода dbus-send на станции (замер 2026-11-23): ключи
    // интерфейсов и свойств печатаются строками `string "<имя>"`; срез до значимого.
    const OFF: &str = "method return time=1795442890.459529 sender=:1.2 -> destination=:1.268 serial=2 reply_serial=2
   array [
      dict entry(
         object path \"/org/bluez\"
         array [
            dict entry(
               string \"org.bluez.AgentManager1\"
               array [
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0\"
         array [
            dict entry(
               string \"org.bluez.Adapter1\"
               array [
                  dict entry(
                     string \"Address\"
                     variant                         string \"AA:BB:CC:DD:EE:FF\"
                  )
                  dict entry(
                     string \"Discoverable\"
                     variant                         boolean false
                  )
                  dict entry(
                     string \"Name\"
                     variant                         string \"Yandex.Mini2\"
                  )
                  dict entry(
                     string \"UUIDs\"
                     variant                         array [
                           string \"00001112-0000-1000-8000-00805f9b34fb\"
                     ]
                  )
               ]
            )
         ]
      )
   ]";

    const DISCOVERABLE: &str = "method return time=1795442890.459529 sender=:1.2 -> destination=:1.268 serial=3 reply_serial=2
   array [
      dict entry(
         object path \"/org/bluez/hci0\"
         array [
            dict entry(
               string \"org.bluez.Adapter1\"
               array [
                  dict entry(
                     string \"Discoverable\"
                     variant                         boolean true
                  )
                  dict entry(
                     string \"DiscoverableTimeout\"
                     variant                         uint32 0
                  )
               ]
            )
         ]
      )
   ]";

    const BOTH_PLAYING: &str = "method return time=1795442890.459529 sender=:1.2 -> destination=:1.268 serial=6 reply_serial=2
   array [
      dict entry(
         object path \"/org/bluez/hci0\"
         array [
            dict entry(
               string \"org.bluez.Adapter1\"
               array [
                  dict entry(
                     string \"Discoverable\"
                     variant                         boolean true
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66\"
         array [
            dict entry(
               string \"org.bluez.Device1\"
               array [
                  dict entry(
                     string \"Connected\"
                     variant                         boolean true
                  )
                  dict entry(
                     string \"Name\"
                     variant                         string \"Phone\"
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66/player0\"
         array [
            dict entry(
               string \"org.bluez.MediaPlayer1\"
               array [
                  dict entry(
                     string \"Status\"
                     variant                         string \"playing\"
                  )
               ]
            )
         ]
      )
   ]";

    const BOTH: &str = "method return time=1795442890.459529 sender=:1.2 -> destination=:1.268 serial=5 reply_serial=2
   array [
      dict entry(
         object path \"/org/bluez/hci0\"
         array [
            dict entry(
               string \"org.bluez.Adapter1\"
               array [
                  dict entry(
                     string \"Discoverable\"
                     variant                         boolean true
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66\"
         array [
            dict entry(
               string \"org.bluez.Device1\"
               array [
                  dict entry(
                     string \"Connected\"
                     variant                         boolean true
                  )
                  dict entry(
                     string \"Name\"
                     variant                         string \"Phone\"
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66/player0\"
         array [
            dict entry(
               string \"org.bluez.MediaPlayer1\"
               array [
                  dict entry(
                     string \"Status\"
                     variant                         string \"paused\"
                  )
               ]
            )
         ]
      )
   ]";

    const CONNECTED: &str = "method return time=1795442890.459529 sender=:1.2 -> destination=:1.268 serial=4 reply_serial=2
   array [
      dict entry(
         object path \"/org/bluez/hci0\"
         array [
            dict entry(
               string \"org.bluez.Adapter1\"
               array [
                  dict entry(
                     string \"Discoverable\"
                     variant                         boolean false
                  )
                  dict entry(
                     string \"Powered\"
                     variant                         boolean true
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66\"
         array [
            dict entry(
               string \"org.bluez.Device1\"
               array [
                  dict entry(
                     string \"Connected\"
                     variant                         boolean true
                  )
                  dict entry(
                     string \"Name\"
                     variant                         string \"Phone\"
                  )
                  dict entry(
                     string \"UUIDs\"
                     variant                         array [
                           string \"0000110b-0000-1000-8000-00805f9b34fb\"
                           string \"0000110c-0000-1000-8000-00805f9b34fb\"
                     ]
                  )
               ]
            )
            dict entry(
               string \"org.bluez.Battery1\"
               array [
                  dict entry(
                     string \"Name\"
                     variant                         string \"battery\"
                  )
               ]
            )
         ]
      )
      dict entry(
         object path \"/org/bluez/hci0/dev_11_22_33_44_55_66/player0\"
         array [
            dict entry(
               string \"org.bluez.MediaPlayer1\"
               array [
                  dict entry(
                     string \"Status\"
                     variant                         string \"playing\"
                  )
                  dict entry(
                     string \"Repeat\"
                     variant                         string \"off\"
                  )
               ]
            )
         ]
      )
   ]";

    fn snap_text(text: &str) -> Snap {
        parse_managed(text)
    }

    #[test]
    fn parses_managed_objects_of_the_real_dbus_send_shape() {
        let s = snap_text(CONNECTED);
        assert!(!s.discoverable);
        assert_eq!(s.devices.len(), 1);
        let d = &s.devices[0];
        assert_eq!(d.path, "/org/bluez/hci0/dev_11_22_33_44_55_66");
        assert!(d.connected);
        assert_eq!(d.name, "Phone");
        // Status приехал из /player0 того же адреса; Repeat проигнорирован.
        assert_eq!(d.status, "playing");
    }

    #[test]
    fn parses_nothing_from_the_idle_adapter() {
        let s = snap_text(OFF);
        assert!(!s.discoverable);
        assert!(s.devices.is_empty());
        assert_eq!(status_line(&s), "off");
    }

    #[test]
    fn status_line_matrix() {
        assert_eq!(status_line(&snap_text(DISCOVERABLE)), "discoverable");
        assert_eq!(
            status_line(&snap_text(CONNECTED)),
            "connected: Phone (playing)"
        );
        let mut both = snap_text(CONNECTED);
        both.discoverable = true;
        assert_eq!(
            status_line(&both),
            "discoverable, connected: Phone (playing)"
        );
        let mut nameless = snap_text(CONNECTED);
        nameless.devices[0].name.clear();
        nameless.devices[0].status.clear();
        assert_eq!(status_line(&nameless), "connected: device");
    }

    #[test]
    fn avr_uses_the_player_of_the_connected_device() {
        assert_eq!(BtAction::parse("next").unwrap().method(), "Next");
        assert_eq!(BtAction::parse("previous").unwrap().method(), "Previous");
        assert!(BtAction::parse("volume").is_none());
    }

    fn local() -> tokio::task::LocalSet {
        tokio::task::LocalSet::new()
    }

    #[test]
    fn actor_flow_mode_disconnect_avr() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        local().block_on(&rt, async {
            tokio::time::pause(); // опросы идут только когда всем нечего делать
            let bus = Rc::new(Bus {
                answers: RefCell::new(VecDeque::from([OFF.to_owned(), DISCOVERABLE.to_owned()])),
                last: RefCell::new(None),
                calls: RefCell::new(Vec::new()),
            });
            let held = bus.clone();
            let call: CallFn = Box::new(move |args: Vec<String>| {
                let bus = held.clone();
                Box::pin(async move {
                    let is_poll = args.iter().any(|a| a.contains("GetManagedObjects"));
                    if is_poll {
                        if let Some(t) = bus.answers.borrow_mut().pop_front() {
                            *bus.last.borrow_mut() = Some(t.clone());
                            return Ok(t);
                        }
                        return Ok(bus.last.borrow().clone().unwrap_or_default());
                    }
                    bus.calls.borrow_mut().push(args.clone());
                    Ok("method return time=9 serial=9 reply_serial=9".to_owned())
                })
            });
            let (h, mut rx) = spawn_with(call);
            // Первый опрос: "" -> "off", Changed; до него handle пуст.
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "off");
            assert!(!h.discoverable());

            h.set_discoverable(true);
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "discoverable");
            assert!(h.discoverable());

            // Источник появился сам (автопереподключение при выключенном режиме):
            // следующий снимок CONNECTED — через команду, чтобы опрос был детерминированным.
            bus.answers.borrow_mut().push_back(CONNECTED.to_owned());
            h.set_discoverable(true);
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "connected: Phone (playing)");
            // Режим не был включён — off не рвёт источник, подключённый сам: Disconnect нет.
            bus.answers.borrow_mut().push_back(BOTH.to_owned());
            h.set_discoverable(false);
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "discoverable, connected: Phone (paused)");
            {
                let calls = bus.calls.borrow();
                assert!(
                    calls
                        .iter()
                        .any(|c| c.iter().any(|a| a == "variant:boolean:false"))
                );
                assert!(
                    !calls.iter().any(|c| c
                        .iter()
                        .any(|a| a.ends_with("org.bluez.Device1.Disconnect"))),
                    "unexpected disconnect: {:?}",
                    *calls
                );
            }

            // Включённый режим, затем выключение — теперь Disconnect подключённого обязан быть.
            bus.answers.borrow_mut().push_back(BOTH_PLAYING.to_owned());
            h.set_discoverable(true);
            rx.recv().await.unwrap();
            bus.answers.borrow_mut().push_back(CONNECTED.to_owned());
            h.set_discoverable(false);
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "connected: Phone (playing)");
            {
                let calls = bus.calls.borrow();
                assert!(
                    calls.iter().any(|c| {
                        c.iter()
                            .any(|a| a.ends_with("org.bluez.Device1.Disconnect"))
                            && c.iter()
                                .any(|a| a == "/org/bluez/hci0/dev_11_22_33_44_55_66")
                    }),
                    "disconnect: {:?}",
                    *calls
                );
            }

            // AVRCP на player0 подключённого устройства, опрос после команды.
            bus.answers.borrow_mut().push_back(DISCOVERABLE.to_owned());
            h.avr(BtAction::Next);
            rx.recv().await.unwrap();
            assert_eq!(h.status(), "discoverable");
            {
                let calls = bus.calls.borrow();
                assert!(
                    calls.iter().any(|c| {
                        c.iter()
                            .any(|a| a == "/org/bluez/hci0/dev_11_22_33_44_55_66/player0")
                            && c.iter().any(|a| a.ends_with("org.bluez.MediaPlayer1.Next"))
                    }),
                    "next: {:?}",
                    *calls
                );
            }
            // handle-ов больше нет — актор завершится сам (п. 5).
            drop(h);
        });
    }
}
