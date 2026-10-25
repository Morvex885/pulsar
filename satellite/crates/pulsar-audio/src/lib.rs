//! Microphone front-end of the daemon: ALSA capture -> Yandex VQE -> wake word and command
//! spotters -> 16 kHz stream, and music features of the speaker reference. The Yandex engines
//! come from the firmware's libyandex_evil.so (C API, loaded with dlopen; never copied into the
//! repository).
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod engine;
mod frontend;
mod spectrum;

pub use frontend::{FrontendEvent, FrontendHandle, FrontendStats, spawn};
pub use spectrum::SpectrumAnalyzer;
