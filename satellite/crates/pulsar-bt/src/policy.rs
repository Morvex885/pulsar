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
            self.deadline = None;
            self.release();
        } else if self.owner.is_none() {
            if let Some(paired) = paired {
                self.known = paired;
            }
            self.deadline = Some(now + PAIRING_TIME);
        }
    }

    pub fn expire(&mut self, now: Instant) {
        if self.deadline.is_some_and(|at| now >= at) {
            self.deadline = None;
        }
    }

    pub fn observe_pairs(&mut self, paired: &BTreeSet<String>) {
        if self.deadline.is_some() {
            self.known.clone_from(paired);
        } else {
            self.known.retain(|p| paired.contains(p));
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

    pub fn select(&mut self, path: String) {
        self.owner = Some(path);
        self.deadline = None;
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
        if self.owner.is_none() || generation != self.generation || self.playback == phase {
            return;
        }
        self.playback = phase;
        self.admitted = false;
        // Смена источника статуса AVRCP → PCM не делает старый фронт новым.
        if phase == Some(Playback::Playing)
            && let Some(start) = order
            && start > self.starts
        {
            self.starts = start;
            self.admitted = true;
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
        p.observe_pairs(&BTreeSet::from(["first".into(), "late".into()]));
        assert!(p.permits("first"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn on_during_outage_does_not_forget_existing_pairs() {
        let now = Instant::now();
        let mut p = Policy::restore(false, BTreeSet::from(["first".into()]));
        p.set_enabled(true, now, None);
        p.expire(now + PAIRING_TIME);
        p.observe_pairs(&BTreeSet::from(["first".into(), "late".into()]));
        assert!(p.permits("first"));
        assert!(!p.permits("late"));
    }

    #[test]
    fn preemption_needs_a_new_start_and_off_rejects_old_frames() {
        let known = BTreeSet::from(["first".into()]);
        let mut p = Policy::restore(true, known);
        p.select("first".into());
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
    fn first_phone_keeps_ownership_when_paused_even_if_second_has_older_sequence() {
        let known = BTreeSet::from(["first".into(), "second".into()]);
        let mut p = Policy::restore(true, known);
        assert_eq!(
            p.choose([("second", 8), ("first", 7)].into_iter()),
            Some("first")
        );
        p.select("first".into());
        p.observe_playback(p.generation, Some(Playback::Paused), None);
        assert_eq!(p.choose([("second", 5)].into_iter()), None);
        p.release();
        assert_eq!(p.choose([("second", 5)].into_iter()), Some("second"));
    }
}
