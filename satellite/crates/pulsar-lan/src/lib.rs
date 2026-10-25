//! LAN services of the daemon: the mDNS responder through which Home Assistant and Music
//! Assistant find the station, and the wake word arbiter between stations on one LAN.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod arbiter;
pub mod mdns;
#[allow(unsafe_code)]
mod sys;

/// strerror text of an OS error, as the C++ daemon logged it (without " (os error N)").
fn errstr(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}
