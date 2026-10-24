//! Playback through the firmware's GStreamer (channels Music / Announce) and the master volume
//! (ALSA "Master Playback Volume", softvol of the `speakers` PCM).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[allow(unsafe_code)]
mod gst;
pub mod mixer;
mod player;

pub use mixer::Mixer;
pub use player::{Channel, PlayId, PlayState, PlayerEvent, PlayerHandle, spawn};
