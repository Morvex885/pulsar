use std::collections::BTreeSet;
use std::time::Duration;

use tokio::time::Instant;

use crate::Playback;

const PAIRING_TIME: Duration = Duration::from_secs(120);

#[derive(Default)]
pub(crate) struct Policy {
    pub enabled: bool,
    pub deadline: Option<Instant>,
    pub owner: Option<String>,
    pub connection: Option<u64>,
    pub generation: u64,
    pub playback: Option<Playback>,
    pub starts: u64,
    pub admitted: bool,
    known: BTreeSet<String>,
    pairing_window: Option<(Instant, Instant)>,
}

impl Policy {
    pub fn restore(enabled: bool, paired: BTreeSet<String>) -> Self {
        Self {
            enabled,
            known: paired,
            ..Self::default()
        }
    }

    pub fn seed_known(&mut self, paired: BTreeSet<String>) {
        self.known = paired;
    }

    pub fn set_enabled(&mut self, on: bool, now: Instant, paired: Option<BTreeSet<String>>) {
        self.enabled = on;
        if !on {
            self.close_pairing(now);
            self.release();
        } else if self.owner.is_none() {
            if let Some(paired) = paired {
                self.known = paired;
            }
            let first = self
                .pairing_window
                .filter(|&(_, end)| now < end)
                .map_or(now, |(first, _)| first);
            let end = now + PAIRING_TIME;
            self.pairing_window = Some((first, end));
            self.deadline = Some(end);
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.deadline.is_some_and(|at| now >= at) {
            self.deadline = None;
        }
    }

    pub fn close_pairing(&mut self, now: Instant) {
        self.deadline = None;
        if let Some((_, end)) = &mut self.pairing_window {
            *end = (*end).min(now);
        }
    }

    pub fn observe_pairs(&mut self, mut paired: impl FnMut(&str) -> bool) {
        self.known.retain(|path| paired(path));
    }

    pub fn observe_pair(&mut self, device: String, received: Instant) {
        if self
            .pairing_window
            .is_some_and(|(first, end)| first <= received && received < end)
        {
            self.known.insert(device);
        }
    }

    pub fn permits(&self, device: &str) -> bool {
        self.enabled && self.known.contains(device)
    }

    pub fn choose<'a>(&self, candidates: impl Iterator<Item = (&'a str, u32)>) -> Option<&'a str> {
        if self.owner.is_some() {
            return None;
        }
        candidates
            .filter(|(device, _)| self.permits(device))
            .min_by_key(|(device, sequence)| (*sequence, *device))
            .map(|(device, _)| device)
    }

    pub fn select(&mut self, path: String, now: Instant) {
        self.owner = Some(path);
        self.close_pairing(now);
        self.new_generation();
        self.connection = Some(self.generation);
    }

    pub fn new_generation(&mut self) {
        self.generation += 1;
        self.playback = None;
        self.admitted = false;
    }

    pub fn release(&mut self) {
        if self.owner.take().is_some() {
            self.connection = None;
            self.new_generation();
        }
    }

    pub fn observe_playback(
        &mut self,
        generation: u64,
        phase: Option<Playback>,
        order: Option<u64>,
    ) {
        if self.owner.is_none() || generation != self.generation {
            return;
        }
        if self.playback != phase {
            self.playback = phase;
            self.admitted = false;
        }
        // Смена источника статуса AVRCP → PCM не делает старый фронт новым.
        if phase == Some(Playback::Playing)
            && let Some(start) = order
            && start > self.starts
        {
            self.starts = start;
            self.admitted = true;
        }
    }

    pub(super) fn reauthorize_playback(
        &mut self,
        observed: u64,
        enabled_at: u64,
        preempt_before: u64,
    ) {
        // Явный Off → On может вернуть прежний допуск, но не вытеснить
        // локальную музыку, начавшуюся после наблюдаемого старта телефона.
        if observed >= preempt_before {
            self.observe_playback(
                self.generation,
                Some(Playback::Playing),
                Some(observed.max(enabled_at)),
            );
        }
    }

    pub fn ready_start(&self, pcm_ready: bool, preempt_before: u64) -> Option<u64> {
        (pcm_ready && self.admitted && self.starts >= preempt_before).then_some(self.starts)
    }

    pub fn preempt_before(&mut self, order: u64) -> bool {
        if self.admitted && self.starts < order {
            self.admitted = false;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_and_expiry_do_not_authorize_a_new_pair() {
        let now = Instant::now();
        let original = BTreeSet::from(["first".into()]);
        let mut p = Policy::restore(true, original.clone());
        assert!(p.deadline.is_none());
        p.set_enabled(true, now, Some(original));
        p.expire(now + Duration::from_secs(119));
        assert!(p.deadline.is_some());
        p.expire(now + PAIRING_TIME);
        p.observe_pairs(|path| matches!(path, "first" | "late"));
        assert!(p.permits("first"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn pairing_uses_receive_time_even_after_window_expires() {
        let now = Instant::now();
        let end = now + PAIRING_TIME;
        let mut p = Policy::restore(false, BTreeSet::new());
        p.set_enabled(true, now, None);
        p.expire(end);
        p.observe_pair("early".into(), now - Duration::from_millis(1));
        p.observe_pair("timely".into(), end - Duration::from_millis(1));
        p.observe_pair("late".into(), end);
        assert!(!p.permits("early"));
        assert!(p.permits("timely"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn explicit_close_still_counts_a_pair_received_before_close() {
        let now = Instant::now();
        let closed = now + Duration::from_secs(1);
        let mut p = Policy::restore(true, BTreeSet::from(["first".into()]));
        p.set_enabled(true, now, None);
        p.select("first".into(), closed);
        p.observe_pair("timely".into(), now);
        p.observe_pair("late".into(), closed);
        assert!(p.deadline.is_none());
        assert!(p.permits("timely"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn on_during_outage_does_not_forget_existing_pairs() {
        let now = Instant::now();
        let mut p = Policy::restore(false, BTreeSet::from(["first".into()]));
        p.set_enabled(true, now, None);
        p.expire(now + PAIRING_TIME);
        p.observe_pairs(|path| matches!(path, "first" | "late"));
        assert!(p.permits("first"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn preemption_needs_a_new_start_and_off_rejects_old_frames() {
        let known = BTreeSet::from(["first".into()]);
        let mut p = Policy::restore(true, known);
        p.select("first".into(), Instant::now());
        let generation = p.generation;
        let start = pulsar_core::audio_start_order();
        let local_start = pulsar_core::audio_start_order();
        p.observe_playback(generation, Some(Playback::Playing), Some(start));
        assert_eq!(p.starts, start);
        assert!(p.starts < local_start);
        assert!(p.preempt_before(local_start));
        p.observe_playback(generation, Some(Playback::Playing), None);
        assert!(!p.admitted);
        p.observe_playback(generation, Some(Playback::Paused), None);
        p.observe_playback(generation, Some(Playback::Playing), Some(start));
        assert!(!p.admitted);
        p.observe_playback(generation, Some(Playback::Paused), None);
        let restart = pulsar_core::audio_start_order();
        p.observe_playback(generation, Some(Playback::Playing), Some(restart));
        assert!(!p.preempt_before(local_start));
        assert!(p.admitted);
        p.set_enabled(false, Instant::now(), None);
        p.observe_playback(generation, Some(Playback::Playing), None);
        assert!(!p.admitted);
        assert!(!p.permits("first"));
    }

    #[test]
    fn explicit_reenable_readmits_cached_playing_without_overriding_local_music() {
        let now = Instant::now();
        for (preempted, expected) in [(0, Some(30)), (20, None)] {
            let mut p = Policy::restore(true, BTreeSet::from(["phone".into()]));
            p.select("phone".into(), now);
            let old = p.generation;
            p.observe_playback(old, Some(Playback::Playing), Some(10));
            p.preempt_before(preempted);
            p.set_enabled(false, now, None);
            p.set_enabled(true, now, None);
            p.select("phone".into(), now);
            p.observe_playback(old, Some(Playback::Playing), Some(99));
            p.observe_playback(p.generation, Some(Playback::Playing), Some(10));
            assert_eq!(p.ready_start(true, preempted), None);

            p.reauthorize_playback(10, 30, preempted);
            assert_eq!(p.ready_start(true, preempted), expected);
            p.new_generation();
            p.observe_playback(p.generation, Some(Playback::Playing), Some(10));
            assert_eq!(p.ready_start(true, preempted), None);
        }
    }

    #[test]
    fn first_phone_keeps_ownership_when_paused_even_if_second_has_older_sequence() {
        let known = BTreeSet::from(["first".into(), "second".into()]);
        let mut p = Policy::restore(true, known);
        assert_eq!(
            p.choose([("second", 8), ("first", 7)].into_iter()),
            Some("first")
        );
        p.select("first".into(), Instant::now());
        p.observe_playback(p.generation, Some(Playback::Paused), None);
        assert_eq!(p.choose([("second", 5)].into_iter()), None);
        p.release();
        assert_eq!(p.choose([("second", 5)].into_iter()), Some("second"));
    }
}
