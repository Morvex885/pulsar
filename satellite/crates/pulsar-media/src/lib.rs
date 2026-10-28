//! Playback through the firmware's GStreamer (channels Music / Announce; Announce is played by
//! our own ALSA writer) and the master volume
//! (ALSA "Master Playback Volume", softvol of the model's playback PCM).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

#[allow(unsafe_code)]
mod gst;
pub mod mixer;
mod pcm_out;
mod player;

pub use mixer::Mixer;
pub use player::{Channel, PlayId, PlayState, PlayerEvent, PlayerHandle, spawn};
