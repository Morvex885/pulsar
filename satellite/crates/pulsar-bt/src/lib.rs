//! Bluetooth-режим колонки поверх штатного стека прошивки: BlueZ (org.bluez) принимает
//! сопряжение и A2DP-поток, blue-alsa выводит звук на усилитель, всё постоянное делает
//! `dbus-send` из актора — см. docs/bluetooth.md. Адаптер ищется в sysfs один раз при старте
//! демона: на моделях без BT-чипа узла нет и сущностей нет. Handle-ы не блокируют
//! (docs/architecture.md, «Модель исполнения», п. 2).

#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod service;

use std::path::Path;

pub use service::{BtEvent, BtHandle, spawn};

/// Команды AVRCP-пульта, которые колонка отправляет подключённому источнику (телефону).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtAction {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

impl BtAction {
    /// Строка аргумента действия `bt`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "play" => Self::Play,
            "pause" => Self::Pause,
            "stop" => Self::Stop,
            "next" => Self::Next,
            "prev" | "previous" => Self::Previous,
            _ => return None,
        })
    }

    /// Метод org.bluez.MediaPlayer1 (doc/media-api.rst BlueZ).
    pub(crate) fn method(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::Pause => "Pause",
            Self::Stop => "Stop",
            Self::Next => "Next",
            Self::Previous => "Previous",
        }
    }
}

/// Есть ли на станции BT-адаптер прошивки: sysfs-узел hci0 создаёт `hciattach` из init-скриптов
/// стека, который pulsar не глушит; на моделях без чипа узла нет.
pub fn adapter_present() -> bool {
    Path::new("/sys/class/bluetooth/hci0").exists()
}
