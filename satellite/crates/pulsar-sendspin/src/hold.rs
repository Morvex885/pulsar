//! Reported stream state. Music Assistant ends and restarts the stream on every seek / skip
//! (~10 ms apart); a stop or pause comes with group state "stopped" (just before or just after
//! the stream end, seen on a Midi). A stream end without it is held this long, so a restart does
//! not flap the HA media state and the music displays. The audio itself stops at once (the sink
//! is released on every end).

use std::cell::Cell;
use std::rc::Rc;

use log::info;
use pulsar_core::Timers;

/// Timers of the service actor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Key {
    /// `SendspinClient::loop()` cadence (sendspin-cpp integration guide).
    Tick,
    /// Held stream end.
    StreamEnd,
}

pub(crate) const STREAM_END_HOLD_MS: u64 = 1000;

/// Each method returns the `Stream(..)` event to send, if any; `streaming` (the handle's getter)
/// is already updated when it returns.
pub(crate) struct StreamHold {
    streaming: Rc<Cell<bool>>,
    group_stopped: bool,
}

impl StreamHold {
    pub(crate) fn new(streaming: Rc<Cell<bool>>) -> Self {
        Self {
            streaming,
            group_stopped: false,
        }
    }

    pub(crate) fn start(&mut self, timers: &mut Timers<Key>) -> Option<bool> {
        self.group_stopped = false;
        timers.cancel(Key::StreamEnd);
        if self.streaming.get() {
            return None;
        }
        self.streaming.set(true);
        Some(true)
    }

    /// `hold` = false: the owner is tearing the service down, end at once.
    pub(crate) fn end(&mut self, timers: &mut Timers<Key>, hold: bool) -> Option<bool> {
        if !self.streaming.get() || timers.is_set(Key::StreamEnd) {
            return None;
        }
        if self.group_stopped || !hold {
            return self.finish(timers);
        }
        timers.set(Key::StreamEnd, STREAM_END_HOLD_MS);
        None
    }

    /// A group update that carries a playback state.
    pub(crate) fn group(&mut self, playing: bool, timers: &mut Timers<Key>) -> Option<bool> {
        self.group_stopped = !playing;
        if self.group_stopped && timers.is_set(Key::StreamEnd) {
            return self.finish(timers);
        }
        None
    }

    /// The hold ran out, or the server disconnected.
    pub(crate) fn finish(&mut self, timers: &mut Timers<Key>) -> Option<bool> {
        timers.cancel(Key::StreamEnd);
        if !self.streaming.get() {
            return None;
        }
        info!("sendspin: stream end");
        self.streaming.set(false);
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::{Instant, advance, timeout};

    use super::*;

    struct Rig {
        timers: Timers<Key>,
        hold: StreamHold,
        streaming: Rc<Cell<bool>>,
    }

    impl Rig {
        fn new() -> Self {
            let streaming = Rc::new(Cell::new(false));
            Self {
                timers: Timers::new(),
                hold: StreamHold::new(streaming.clone()),
                streaming,
            }
        }

        /// What the actor does when the timer fires: None if nothing fires within `within`.
        async fn fire(&mut self, within: Duration) -> Option<Option<bool>> {
            match timeout(within, self.timers.next()).await {
                Ok(Key::StreamEnd) => Some(self.hold.finish(&mut self.timers)),
                Ok(Key::Tick) => unreachable!(),
                Err(_) => None,
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn end_without_stopped_is_held_one_second() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert!(r.streaming.get());
        let t0 = Instant::now();
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        assert!(r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(Some(false)));
        assert_eq!(t0.elapsed(), Duration::from_millis(STREAM_END_HOLD_MS));
        assert!(!r.streaming.get());
        // A second end (sendspin-cpp's stop()) changes nothing.
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_within_10_ms_keeps_streaming() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        advance(Duration::from_millis(10)).await;
        assert_eq!(r.hold.start(&mut r.timers), None);
        assert!(r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
        assert!(r.streaming.get());
        // The next end is held again from its own time.
        let t0 = Instant::now();
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(Some(false)));
        assert_eq!(t0.elapsed(), Duration::from_millis(STREAM_END_HOLD_MS));
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_before_end_ends_at_once() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        // "playing" changes nothing; "stopped" without a held end only marks the group.
        assert_eq!(r.hold.group(true, &mut r.timers), None);
        assert_eq!(r.hold.group(false, &mut r.timers), None);
        assert!(r.streaming.get());
        assert_eq!(r.hold.end(&mut r.timers, true), Some(false));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_after_end_finishes_the_hold() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        advance(Duration::from_millis(300)).await;
        assert!(r.streaming.get());
        assert_eq!(r.hold.group(false, &mut r.timers), Some(false));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn new_stream_clears_stopped() {
        let mut r = Rig::new();
        assert_eq!(r.hold.group(false, &mut r.timers), None);
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        // The stop belonged to the previous stream: this end is held.
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        assert!(r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(Some(false)));
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_ends_a_held_stream_at_once() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert_eq!(r.hold.end(&mut r.timers, true), None);
        advance(Duration::from_millis(100)).await;
        assert_eq!(r.hold.finish(&mut r.timers), Some(false));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
        // Disconnect while streaming (no end yet) also ends it; idle: nothing.
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert_eq!(r.hold.finish(&mut r.timers), Some(false));
        assert_eq!(r.hold.finish(&mut r.timers), None);
    }

    #[tokio::test(start_paused = true)]
    async fn teardown_does_not_hold() {
        let mut r = Rig::new();
        assert_eq!(r.hold.start(&mut r.timers), Some(true));
        assert_eq!(r.hold.end(&mut r.timers, false), Some(false));
        assert!(!r.streaming.get());
    }
}
