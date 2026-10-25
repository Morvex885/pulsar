//! Per-actor timers keyed by the actor's own enum, with the firing order of the C++ `Loop`:
//! every wake-up takes a snapshot of the due keys and returns them in the order they were last
//! armed; a key cancelled or re-armed by the handling of an earlier key of the same snapshot is
//! skipped. Actors rely on this order where two timers come due on the same tick.
//!
//! The C++ loop ran one snapshot's callbacks back to back. Here that holds only if the actor
//! polls `timers.next()` as the FIRST branch of a `biased;` `tokio::select!`: otherwise another
//! ready branch (a command, an event) can run between two keys of the same snapshot.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

/// Stand-in for a deadline past the end of `Instant`: never, for a daemon.
const FAR_FUTURE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// `t + d`, saturating to ~30 years ahead: an overflow panic would abort the daemon.
fn after(t: Instant, d: Duration) -> Instant {
    t.checked_add(d)
        .or_else(|| t.checked_add(FAR_FUTURE))
        .unwrap_or(t)
}

struct Entry<K> {
    key: K,
    due: Instant,
    period: Option<Duration>,
    /// Arming order; a repeating timer keeps its number across periods, like a C++ timer id.
    seq: u64,
}

pub struct Timers<K: Copy + Eq> {
    entries: Vec<Entry<K>>,
    next_seq: u64,
    /// Due keys of the last wake-up not yet returned, with the arming they were due under.
    snapshot: VecDeque<(K, u64)>,
    snapshot_at: Instant,
}

impl<K: Copy + Eq> Default for Timers<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq> Timers<K> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_seq: 0,
            snapshot: VecDeque::new(),
            snapshot_at: Instant::now(),
        }
    }

    /// (Re)arms `key` to fire once after `delay_ms`.
    pub fn set(&mut self, key: K, delay_ms: u64) {
        self.arm(key, delay_ms, None);
    }

    /// (Re)arms `key` to fire after `delay_ms`, then every `period_ms` (counted from the wake-up
    /// that fired it). `period_ms == 0` is a one-shot, as in the C++ loop.
    pub fn set_repeating(&mut self, key: K, delay_ms: u64, period_ms: u64) {
        let period = (period_ms > 0).then(|| Duration::from_millis(period_ms));
        self.arm(key, delay_ms, period);
    }

    pub fn cancel(&mut self, key: K) {
        self.entries.retain(|e| e.key != key);
    }

    pub fn is_set(&self, key: K) -> bool {
        self.entries.iter().any(|e| e.key == key)
    }

    /// Waits for the next due key; pending forever while nothing is armed. Cancel-safe: state
    /// only changes between the wake-up and the return, with no await in between.
    pub async fn next(&mut self) -> K {
        loop {
            if let Some(key) = self.pop_due() {
                return key;
            }
            let Some(due) = self.entries.iter().map(|e| e.due).min() else {
                return std::future::pending().await;
            };
            tokio::time::sleep_until(due).await;
            self.take_snapshot(Instant::now());
        }
    }

    fn arm(&mut self, key: K, delay_ms: u64, period: Option<Duration>) {
        self.cancel(key);
        let seq = self.next_seq;
        self.next_seq += 1;
        let due = after(Instant::now(), Duration::from_millis(delay_ms));
        self.entries.push(Entry {
            key,
            due,
            period,
            seq,
        });
    }

    fn take_snapshot(&mut self, now: Instant) {
        let mut due: Vec<_> = self
            .entries
            .iter()
            .filter(|e| e.due <= now)
            .map(|e| (e.key, e.seq))
            .collect();
        due.sort_unstable_by_key(|&(_, seq)| seq);
        self.snapshot.extend(due);
        self.snapshot_at = now;
    }

    fn pop_due(&mut self) -> Option<K> {
        while let Some((key, seq)) = self.snapshot.pop_front() {
            let Some(i) = self
                .entries
                .iter()
                .position(|e| e.key == key && e.seq == seq)
            else {
                continue;
            };
            match self.entries[i].period {
                Some(period) => self.entries[i].due = after(self.snapshot_at, period),
                None => {
                    self.entries.swap_remove(i);
                }
            }
            return Some(key);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{advance, timeout};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum K {
        A,
        B,
        C,
    }

    fn ms(start: Instant) -> u128 {
        (Instant::now() - start).as_millis()
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_fires_in_last_arm_order() {
        let start = Instant::now();
        let mut t = Timers::new();
        t.set(K::A, 10);
        t.set(K::B, 5);
        t.set(K::C, 20);
        t.set(K::A, 1); // re-armed: last, though due first
        advance(Duration::from_millis(30)).await;
        // Not by due time (A, B, C) but by arming (B, C, A).
        assert_eq!(t.next().await, K::B);
        assert_eq!(t.next().await, K::C);
        assert_eq!(t.next().await, K::A);
        assert_eq!(ms(start), 30);
        assert!(!t.is_set(K::A));
    }

    #[tokio::test(start_paused = true)]
    async fn earlier_key_cancels_or_rearms_later_one_in_same_snapshot() {
        let start = Instant::now();
        let mut t = Timers::new();
        t.set(K::A, 10);
        t.set(K::B, 10);
        t.set(K::C, 10);
        assert_eq!(t.next().await, K::A);
        // Handling A cancels B and re-arms C: neither fires from this snapshot.
        t.cancel(K::B);
        t.set(K::C, 50);
        assert_eq!(t.next().await, K::C);
        assert_eq!(ms(start), 60);
        assert!(!t.is_set(K::B));
    }

    #[tokio::test(start_paused = true)]
    async fn key_armed_during_snapshot_waits_for_next_wakeup() {
        let mut t = Timers::new();
        t.set(K::A, 10);
        t.set(K::B, 10);
        assert_eq!(t.next().await, K::A);
        // Due at once, but not part of the running snapshot: B still comes first.
        t.set(K::C, 0);
        assert_eq!(t.next().await, K::B);
        assert_eq!(t.next().await, K::C);
    }

    #[tokio::test(start_paused = true)]
    async fn repeating_rearms_from_wakeup() {
        let start = Instant::now();
        let mut t = Timers::new();
        t.set_repeating(K::A, 10, 50);
        for expected in [10, 60, 110] {
            assert_eq!(t.next().await, K::A);
            assert_eq!(ms(start), expected);
            assert!(t.is_set(K::A));
        }
        // A late handler does not shift the period's base: the wake-up time does.
        advance(Duration::from_millis(70)).await; // now 180, due was 160
        assert_eq!(t.next().await, K::A);
        assert_eq!(ms(start), 180);
        assert_eq!(t.next().await, K::A);
        assert_eq!(ms(start), 230);
        t.cancel(K::A);
        assert!(!t.is_set(K::A));
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_and_empty_is_pending_forever() {
        let mut t = Timers::new();
        t.set(K::A, 10);
        t.cancel(K::A);
        assert!(!t.is_set(K::A));
        assert!(timeout(Duration::from_secs(3600), t.next()).await.is_err());
        t.set(K::B, 10);
        assert_eq!(t.next().await, K::B);
    }

    #[tokio::test(start_paused = true)]
    async fn next_is_cancel_safe() {
        let start = Instant::now();
        let mut t = Timers::new();
        t.set(K::A, 100);
        t.set(K::B, 100);
        tokio::select! {
            _ = t.next() => panic!("timer fired early"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert!(t.is_set(K::A));
        assert_eq!(t.next().await, K::A);
        assert_eq!(t.next().await, K::B);
        assert_eq!(ms(start), 100);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_period_is_one_shot() {
        let start = Instant::now();
        let mut t = Timers::new();
        t.set_repeating(K::A, 10, 0);
        assert_eq!(t.next().await, K::A);
        assert_eq!(ms(start), 10);
        assert!(!t.is_set(K::A));
        assert!(timeout(Duration::from_secs(3600), t.next()).await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn huge_delays_do_not_overflow() {
        let mut t = Timers::new();
        t.set(K::A, u64::MAX);
        t.set_repeating(K::B, 10, u64::MAX);
        assert_eq!(t.next().await, K::B);
        assert!(t.is_set(K::A) && t.is_set(K::B));
        assert!(
            timeout(Duration::from_secs(86_400 * 365), t.next())
                .await
                .is_err()
        );
        assert_eq!(
            after(Instant::now(), Duration::MAX) - Instant::now(),
            FAR_FUTURE
        );
    }
}
