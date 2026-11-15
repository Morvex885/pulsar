//! Воспроизведение через GStreamer прошивки (каналы Music / Announce; Announce выводит свой
//! ALSA-писатель) и мастер-громкость (softvol «Master Playback Volume» на PCM `Model::master_pcm`).
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
