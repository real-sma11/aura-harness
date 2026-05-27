//! Per-turn repeated-read tracker (Phase 3b of the reread-efficiency plan).
//!
//! `RepeatedReadTracker` watches the stream of `content_hash` values
//! attached to read-only tool results inside a single model turn (one
//! request/response round-trip) and queues a [`SteeringKind::RepeatedRead`]
//! nudge whenever any single hash crosses the
//! [`aura_config::REPEATED_READ_THRESHOLD`] occurrences in that turn.
//! The nudge fires on the *next* turn so it lands in the prompt
//! prefix the model actually reads, and at most once per
//! `(turn, content_hash)` pair so a 4th, 5th, … repeat does not spam
//! the model with a fresh nudge inside the same turn.
//!
//! Phase 2 of the core-loop architecture refactor relocated this
//! evaluator out of `prompts/steering/` (where it had no business
//! living once the prompts layer became render-only) and into
//! `agent_loop/steering/`, alongside the other stateful evaluators.

use std::collections::HashMap;

use aura_prompts::SteeringKind;

/// Per-turn occurrence tracker for read-only tool result `content_hash`
/// values. See module-level documentation for the contract.
///
/// The tracker only stores hashes; rendering the nudge body is
/// delegated to [`aura_prompts::SteeringRenderer`] via
/// [`SteeringKind::RepeatedRead`] so wording stays in lockstep with
/// every other steering kind.
#[derive(Debug, Default)]
pub struct RepeatedReadTracker {
    /// `content_hash` → number of times observed in the current turn.
    /// Cleared on every [`Self::begin_turn`] call.
    counts: HashMap<String, usize>,
    /// Hashes whose count crossed the firing threshold in the current
    /// turn. Drained by [`Self::begin_turn`] into the [`SteeringKind`]
    /// vec returned to the caller.
    pending: Vec<String>,
}

impl RepeatedReadTracker {
    /// Construct an empty tracker. The first turn is implicitly
    /// "turn 0" — callers may begin recording immediately, or call
    /// [`Self::begin_turn`] first to drain a (necessarily empty)
    /// initial nudge list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one observation of `content_hash` for the current turn.
    ///
    /// Returns `true` when this call moved the count past the
    /// `aura_config::REPEATED_READ_THRESHOLD` boundary and queued a
    /// nudge for the *next* turn; returns `false` for every other
    /// call (including the 4th, 5th, … repeat in the same turn —
    /// those are absorbed silently so the model receives at most one
    /// nudge per `(turn, content_hash)` pair).
    pub fn record(&mut self, content_hash: &str) -> bool {
        if content_hash.is_empty() {
            return false;
        }
        let count = self.counts.entry(content_hash.to_string()).or_insert(0);
        *count += 1;
        if *count == aura_config::REPEATED_READ_THRESHOLD {
            self.pending.push(content_hash.to_string());
            true
        } else {
            false
        }
    }

    /// Returns the number of nudges currently queued for the next
    /// turn. Used by tests to assert that a 4th-and-later repeat in
    /// the same turn does not enqueue an extra nudge — production
    /// code reads the queue exclusively through [`Self::begin_turn`],
    /// so this accessor is `#[cfg(test)]` only.
    #[cfg(test)]
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Begin a new model turn: drain the queued nudges, render them
    /// as [`SteeringKind`] values, and clear the per-turn counts.
    ///
    /// Callers append the returned [`SteeringKind`]s to the live user
    /// message via [`super::inject::inject`] so they ride into the
    /// prompt prefix the next model request reads.
    pub fn begin_turn(&mut self) -> Vec<SteeringKind> {
        self.counts.clear();
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|hash| SteeringKind::RepeatedRead { content_hash: hash })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_at_threshold_and_only_once_per_turn() {
        let mut tr = RepeatedReadTracker::new();
        let hash = "abc123";
        let threshold = aura_config::REPEATED_READ_THRESHOLD;
        for _ in 0..(threshold - 1) {
            assert!(!tr.record(hash));
        }
        assert!(tr.record(hash));
        assert_eq!(tr.pending_count(), 1);
        // Subsequent repeats inside the same turn are absorbed.
        assert!(!tr.record(hash));
        assert!(!tr.record(hash));
        assert_eq!(tr.pending_count(), 1);
    }

    #[test]
    fn begin_turn_drains_into_steering_kind() {
        let mut tr = RepeatedReadTracker::new();
        for _ in 0..aura_config::REPEATED_READ_THRESHOLD {
            tr.record("x");
        }
        let kinds = tr.begin_turn();
        assert_eq!(kinds.len(), 1);
        match &kinds[0] {
            SteeringKind::RepeatedRead { content_hash } => {
                assert_eq!(content_hash, "x");
            }
            other => panic!("unexpected kind: {other:?}"),
        }
    }

    #[test]
    fn empty_hash_is_ignored() {
        let mut tr = RepeatedReadTracker::new();
        for _ in 0..(aura_config::REPEATED_READ_THRESHOLD + 5) {
            assert!(!tr.record(""));
        }
        assert_eq!(tr.pending_count(), 0);
    }

    #[test]
    fn resets_per_turn_counts() {
        let mut tracker = RepeatedReadTracker::new();
        // Two reads in turn 1 — below threshold.
        tracker.record("hash_a");
        tracker.record("hash_a");
        assert_eq!(tracker.pending_count(), 0);

        // Turn boundary clears the count for hash_a.
        let drained = tracker.begin_turn();
        assert!(drained.is_empty());

        // Two more reads in turn 2 — still below threshold because the
        // counter reset.
        tracker.record("hash_a");
        tracker.record("hash_a");
        assert_eq!(
            tracker.pending_count(),
            0,
            "per-turn counts must reset on begin_turn so repeats only fire when 3 land in one turn"
        );
    }

    #[test]
    fn isolates_distinct_hashes() {
        let mut tracker = RepeatedReadTracker::new();
        for _ in 0..aura_config::REPEATED_READ_THRESHOLD {
            tracker.record("hash_a");
        }
        for _ in 0..(aura_config::REPEATED_READ_THRESHOLD - 1) {
            tracker.record("hash_b");
        }
        let nudges = tracker.begin_turn();
        assert_eq!(
            nudges.len(),
            1,
            "hash_b stayed below threshold, only hash_a should fire"
        );
        match &nudges[0] {
            SteeringKind::RepeatedRead { content_hash } => assert_eq!(content_hash, "hash_a"),
            other => panic!("unexpected steering kind drained from tracker: {other:?}"),
        }
    }
}
