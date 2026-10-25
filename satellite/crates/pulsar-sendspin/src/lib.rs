//! Native Sendspin player (Music Assistant's synchronized streaming protocol) on the official
//! sendspin-cpp client library, through the C shim in `shim/`. Roles: player (own ALSA sink on
//! the `speakers` dmix), metadata, controller (transport commands), visualizer (8 log bands
//! 40 Hz–7.6 kHz, loudness, beats → `MusicFrame`).
//!
//! sendspin-cpp is only linked in the station builds of the Makefile (cfg `sendspin_ffi`, set by
//! build.rs); elsewhere `spawn` fails with "sendspin: built without sendspin-cpp".
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::path::PathBuf;

// The pure parts below are driven only by the sendspin-cpp build (and the tests).
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
mod event;
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
mod gain;
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
mod hold;
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
mod music;
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
mod persist;

#[cfg(sendspin_ffi)]
#[allow(unsafe_code)]
mod ffi;
#[cfg(sendspin_ffi)]
mod service;
#[cfg(sendspin_ffi)]
mod sink;
#[cfg(not(sendspin_ffi))]
mod stub;

#[cfg(sendspin_ffi)]
pub use service::{SendspinHandle, spawn};
#[cfg(not(sendspin_ffi))]
pub use stub::{SendspinHandle, spawn};

pub struct SendspinConfig {
    /// Friendly name shown in Music Assistant.
    pub name: String,
    pub product_name: String,
    pub manufacturer: String,
    pub software_version: String,
    /// "aa:bb:cc:dd:ee:ff"; empty: the library detects it.
    pub mac: String,
    /// Identity / pairing persistence directory (created 0700 if missing, not recursively).
    pub state_dir: PathBuf,
    pub port: u16,
}

/// Group transport commands (controller role).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SendspinEvent {
    /// A stream started (true) / ended (false); see `SendspinHandle::streaming`.
    Stream(bool),
    /// The server set the player volume, 0..1.
    Volume(f32),
    /// The server set the player mute.
    Mute(bool),
    /// Show the pairing code; "" = clear it.
    PairingCode(String),
    /// Ask the operator for the pairing gesture (`confirm_pairing_window`).
    PairingWindow(bool),
    /// Visualizer data from the server.
    Music(pulsar_core::MusicFrame),
}

/// Player volume in percent as the protocol carries it (C++ `lround(clamp(v, 0, 1) * 100)`).
#[cfg_attr(not(sendspin_ffi), allow(dead_code))]
fn volume_percent(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Vectors of the C++ reference's pure functions (tests/golden/cxx_reference.txt): one line per
/// case, a tag, then the values; f32 as their bit pattern in hex.
#[cfg(test)]
mod golden {
    const TEXT: &str = include_str!("../tests/golden/cxx_reference.txt");

    /// The fields after the tag of every line tagged `tag`.
    pub(crate) fn lines(tag: &str) -> Vec<Vec<&'static str>> {
        TEXT.lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                (f.next() == Some(tag)).then(|| f.collect())
            })
            .collect()
    }

    pub(crate) fn f32(hex: &str) -> f32 {
        f32::from_bits(u32::from_str_radix(hex, 16).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_percent_matches_cxx() {
        let cases = golden::lines("volume");
        assert_eq!(cases.len(), 16);
        for f in cases {
            let want: u8 = f[1].parse().unwrap();
            assert_eq!(volume_percent(golden::f32(f[0])), want, "{f:?}");
        }
        assert_eq!(volume_percent(f32::NAN), 0);
    }

    #[test]
    fn spawn_without_sendspin_cpp_fails() {
        if cfg!(sendspin_ffi) {
            return;
        }
        let cfg = SendspinConfig {
            name: "test".into(),
            product_name: String::new(),
            manufacturer: String::new(),
            software_version: String::new(),
            mac: String::new(),
            state_dir: PathBuf::from("/nonexistent"),
            port: 0,
        };
        match spawn(cfg, 0.2, false) {
            Err(e) => assert_eq!(e, "sendspin: built without sendspin-cpp"),
            Ok(_) => panic!("spawned without sendspin-cpp"),
        }
    }
}
