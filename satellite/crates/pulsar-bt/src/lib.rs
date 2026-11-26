//! Входящий A2DP поверх штатных BlueZ и BlueALSA, с управляемым ALSA-выходом.
//! См. docs/bluetooth.md, «Владение аудиовыходом».

#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

mod bus;
mod pcm;
mod policy;
mod service;
mod stock;

pub use service::{BtEvent, BtHandle, BtProbe, BtState, probe, spawn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Playback {
    Playing,
    Paused,
}

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
