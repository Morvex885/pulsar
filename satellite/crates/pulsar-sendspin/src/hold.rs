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

/// Скрывает короткий разрыв потока при перемотке или смене трека. `end`, `group`
/// и `finish` решают, сообщать ли об остановке. Каждый запуск получает новое поколение.
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

    // Новый StreamStart сбрасывает только старое состояние группы. Пока нет записи кадров,
    // отложенный конец предыдущего потока должен сработать как обычно.
    pub(crate) fn starting(&mut self) {
        self.group_stopped = false;
    }

    pub(crate) fn ready(&mut self, timers: &mut Timers<Key>) {
        timers.cancel(Key::StreamEnd);
        self.streaming.set(true);
    }

    /// `hold` = false: the owner is tearing the service down, end at once.
    pub(crate) fn end(&mut self, timers: &mut Timers<Key>, hold: bool) -> bool {
        if !self.streaming.get() || timers.is_set(Key::StreamEnd) {
            return false;
        }
        if self.group_stopped || !hold {
            return self.finish(timers);
        }
        timers.set(Key::StreamEnd, STREAM_END_HOLD_MS);
        false
    }

    /// A group update that carries a playback state.
    pub(crate) fn group(&mut self, playing: bool, timers: &mut Timers<Key>) -> bool {
        self.group_stopped = !playing;
        if self.group_stopped && timers.is_set(Key::StreamEnd) {
            return self.finish(timers);
        }
        false
    }

    /// The hold ran out, or the server disconnected.
    pub(crate) fn finish(&mut self, timers: &mut Timers<Key>) -> bool {
        timers.cancel(Key::StreamEnd);
        if !self.streaming.replace(false) {
            return false;
        }
        info!("sendspin: stream end");
        true
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

        fn start(&mut self) {
            self.hold.starting();
            self.hold.ready(&mut self.timers);
        }

        /// What the actor does when the timer fires: None if nothing fires within `within`.
        async fn fire(&mut self, within: Duration) -> Option<bool> {
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
        r.start();
        assert!(r.streaming.get());
        let t0 = Instant::now();
        assert!(!r.hold.end(&mut r.timers, true));
        assert!(r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(true));
        assert_eq!(t0.elapsed(), Duration::from_millis(STREAM_END_HOLD_MS));
        assert!(!r.streaming.get());
        // A second end (sendspin-cpp's stop()) changes nothing.
        assert!(!r.hold.end(&mut r.timers, true));
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_within_10_ms_keeps_streaming() {
        let mut r = Rig::new();
        r.start();
        assert!(!r.hold.end(&mut r.timers, true));
        advance(Duration::from_millis(10)).await;
        r.start();
        assert!(r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
        assert!(r.streaming.get());
        // The next end is held again from its own time.
        let t0 = Instant::now();
        assert!(!r.hold.end(&mut r.timers, true));
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(true));
        assert_eq!(t0.elapsed(), Duration::from_millis(STREAM_END_HOLD_MS));
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_before_end_ends_at_once() {
        let mut r = Rig::new();
        r.start();
        // "playing" changes nothing; "stopped" without a held end only marks the group.
        assert!(!r.hold.group(true, &mut r.timers));
        assert!(!r.hold.group(false, &mut r.timers));
        assert!(r.streaming.get());
        assert!(r.hold.end(&mut r.timers, true));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn stopped_after_end_finishes_the_hold() {
        let mut r = Rig::new();
        r.start();
        assert!(!r.hold.end(&mut r.timers, true));
        advance(Duration::from_millis(300)).await;
        assert!(r.streaming.get());
        assert!(r.hold.group(false, &mut r.timers));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn new_stream_clears_stopped() {
        let mut r = Rig::new();
        assert!(!r.hold.group(false, &mut r.timers));
        r.start();
        // The stop belonged to the previous stream: this end is held.
        assert!(!r.hold.end(&mut r.timers, true));
        assert_eq!(r.fire(Duration::from_secs(5)).await, Some(true));
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_ends_a_held_stream_at_once() {
        let mut r = Rig::new();
        r.start();
        assert!(!r.hold.end(&mut r.timers, true));
        advance(Duration::from_millis(100)).await;
        assert!(r.hold.finish(&mut r.timers));
        assert!(!r.streaming.get());
        assert_eq!(r.fire(Duration::from_secs(5)).await, None);
        // Disconnect while streaming (no end yet) also ends it; idle: nothing.
        r.start();
        assert!(r.hold.finish(&mut r.timers));
        assert!(!r.hold.finish(&mut r.timers));
    }

    #[tokio::test(start_paused = true)]
    async fn teardown_does_not_hold() {
        let mut r = Rig::new();
        r.start();
        assert!(r.hold.end(&mut r.timers, false));
        assert!(!r.streaming.get());
    }
}
