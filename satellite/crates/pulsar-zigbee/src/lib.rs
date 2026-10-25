//! The Station Midi's Zigbee radio: a Telink B91 on `/dev/ttyS2` (115200 8N1, no flow control)
//! whose reset and boot lines the kernel's `zb_control` driver exports as sysfs value files. It
//! runs one of pulsar's firmwares (`radio/`): the coordinator (a network co-processor for ZHA and
//! Zigbee2MQTT) or the router (a repeater in somebody else's network).
//!
//! The public API is [`spawn`] / [`ZigbeeHandle`] / [`ZigbeeEvent`] (the
//! role service, `service.rs`): it identifies the firmware, writes the role's image when needed and
//! runs the role ([`router::Router`], or [`coordinator::Coordinator`] with
//! [`deconz::DeconzServer`] as its hooks). `zigbee_probe` uses the pieces directly.
//!
//! # Internal API
//!
//! Everything below runs inside the Zigbee service's actor, on the runtime thread; nothing
//! here spawns tasks. The pieces mirror the C++ objects, with the C++ callbacks turned around:
//!
//! - [`link::Link`]: the zbhci serial link (framing, stop-and-wait ACKs, retries, hardware reset).
//!   [`link::Link::open`] (inside the runtime: it registers the tty) opens the tty and starts a
//!   reset; [`link::Link::next`] (cancel-safe) yields [`link::LinkEvent`]s: frames, ACKs of
//!   frames sent with an [`link::AckTag`] (the C++ `AckCb` continuations), and boots of the
//!   radio's application. The link's own timers (reset pulse, boot wait, ACK retries, holds) only
//!   advance while `next()` is polled, so its owner polls it continuously from `open` on, as the
//!   C++ loop always ran. While the service identifies the firmware it owns the link itself
//!   (boot → 0x0900, frame 0x8900 → firmware version).
//! - [`coordinator::Coordinator`] and [`router::Router`] take the link over by value (the C++
//!   role objects replaced the link callbacks in `start()`); events the link has already queued
//!   then go to the role object. Their actor loop is `w = role.next().await` (cancel-safe, may
//!   sit in a `select!` beside other branches) followed by `role.handle(w, …)`.
//! - The coordinator's C++ hooks are [`coordinator::Hooks`], passed into every call that can run
//!   them, with the coordinator itself as an argument: the deCONZ server calls back into
//!   it (`form`, `send`, …) from inside a hook, as the C++ server did. Hooks the C++ code ran
//!   synchronously (`changed`, the `form` result, received indications) run inside the call;
//!   hooks it posted to the loop (send results, the coordinator's own ZDO answers) run from the
//!   next `next()` / `handle()`: after the current dispatch, before the next link event. (C++
//!   ran them after the whole batch of frames read in one go; frames of one read are here
//!   dispatched one per `next()`, with the posted hooks in between.)
//! - Wiring: the deCONZ server implements [`coordinator::Hooks`] (it may
//!   call `coord.form(&cfg, self)` from inside `changed`); the service wraps it, or keeps a
//!   `changed` flag that its `Hooks::changed` sets, and after each `coord.handle(w, …)` reads
//!   the flag to publish its own status.
//! - The router reports C++ `changed()` as the `bool` returned by [`router::Router::join`] and
//!   [`router::Router::handle`].
//! - [`flasher::radio_flash`] is blocking (~25 s for an image) and has no stop flag: run it on
//!   its own thread after dropping the link; at shutdown the owner does not wait for it (the
//!   thread is detached; an upload cut short leaves a silent radio that the next start writes
//!   again).
//!
//! `Mode`, `Role`, `ROLES`, `role` and `parse_mode` are the role table.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod bytes;
pub mod coordinator;
pub mod deconz;
pub mod flasher;
pub mod link;
mod roles;
pub mod router;
mod service;
#[allow(unsafe_code)]
mod sys;
#[cfg(test)]
mod test_util;

pub use roles::{Mode, ROLES, Role, parse_mode, role};
pub use service::{ZigbeeConfig, ZigbeeEvent, ZigbeeHandle, spawn};

/// The radio's UART and the `zb_control` reset / boot value files (active low, 1 = asserted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RadioLines {
    pub tty: String,
    pub reset: String,
    pub boot: String,
}

/// strerror text of an I/O error, as the C++ messages print it (without Rust's "(os error N)").
fn os_error_text(e: &std::io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_owned(),
        None => s,
    }
}
