//! pulsar updates from GitHub releases (Home Assistant "Firmware" update entity): finds the latest
//! stable release, downloads `pulsar-<arch>.bin`, checks it against the release's `SHA256SUMS` and
//! that it runs here (`--version`), then replaces the installed binary (kept as `.prev`). The
//! caller exits the daemon afterwards; `supervise.sh` starts the new binary and rolls back to
//! `.prev` if it crash-loops. HTTPS is rustls on the runtime thread; file work runs on blocking
//! threads.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

/// For update_probe only, not part of the crate's public API.
#[doc(hidden)]
pub mod https;
#[allow(unsafe_code)]
mod sys;
#[cfg(test)]
mod test_server;
mod updater;
mod version;

pub use updater::{Release, UpdateEvent, UpdaterHandle, arch, spawn};
pub use version::release_newer;
