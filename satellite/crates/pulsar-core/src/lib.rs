//! Shared runtime pieces of the pulsar daemon: logging, actor timers and registry, sysfs and
//! network helpers, the per-model hardware description and small types used across services.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod actors;
mod ambient;
pub mod fmt;
mod logger;
pub mod model;
pub mod net;
#[allow(unsafe_code)]
mod sys;
pub mod sysfs;
mod timers;
pub mod tz;

pub use ambient::AmbientDimmer;
pub use logger::{
    LogLine, add_own_target, init_logger, set_log_sink, stderr_level, take_log_lines,
};
pub use timers::Timers;

/// Milliseconds on CLOCK_MONOTONIC (not affected by NTP steps after boot).
pub fn now_ms() -> i64 {
    sys::monotonic_ms()
}

/// Music features for the LED ring and the clock, from either source: the speaker reference of
/// the capture or the Sendspin visualizer role (Music Assistant).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MusicFrame {
    /// 0..1 on a dB scale: 0 = 45 dB under the running peak (speaker reference) / -60 dBFS
    /// (Sendspin), 1 = the peak / 0 dBFS. Bands log-spaced 40 Hz .. 7.6 kHz, low to high.
    pub bands: [f32; 8],
    pub loudness: f32,
    pub beat: bool,
}

/// Sendspin frames win while they keep coming; the speaker reference takes over this long after
/// the last one. Sendspin pauses up to ~0.4 s inside a stream (seen on a Midi, no stream event)
/// and restarts on every seek / skip with ~0.7 s without frames.
pub const SENDSPIN_FRESH_MS: i64 = 500;
/// Frames older than this count as none. Must exceed `SENDSPIN_FRESH_MS` by a frame, or the
/// display drops out before the speaker reference arrives.
pub const MUSIC_STALE_MS: i64 = 600;

/// Voice states shown on the ring and the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiState {
    /// Ring off (or led_show / user light / music), clock shows the time.
    Idle,
    /// Streaming to HA.
    Listening,
    /// Intent processing.
    Thinking,
    /// TTS playing.
    Speaking,
    /// Pipeline error: runs its 3.5 s to the end, then Idle.
    Error,
    /// Timer finished, ringing.
    TimerRing,
}

/// One-shot clock animations (stock Lite 2 `ledpatterns/clock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockShot {
    Boot,
    Pause,
    Next,
    Previous,
}
