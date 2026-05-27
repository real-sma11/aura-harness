//! Per-turn repeated-read tracker (Phase 3b of the reread-efficiency plan).
//!
//! `RepeatedReadTracker` watches the stream of `content_hash` values
//! attached to read-only tool results inside a single model turn (one
//! request/response round-trip) and queues a [`SteeringKind::RepeatedRead`]
//! nudge whenever any single hash crosses **3** identical occurrences in
//! that turn. The nudge fires on the *next* turn so it lands in the
//! prompt prefix the model actually reads, and at most once per
//! `(turn, content_hash)` pair so a 4th, 5th, … repeat does not spam
//! the model with a fresh nudge inside the same turn.
//!
//! The tracker is intentionally a small standalone struct rather than
//! state on [`super::injector::SteeringInjector`] — `SteeringInjector`
//! is documented as a stateless namespace type, and adding cross-turn
//! state to it would change its identity and force every existing
//! call site to take a `&mut`. Callers (the agent loop's per-turn
//! orchestration, future Phase-3 wiring) hold the tracker on their
//! own state and feed it `content_hash` strings off the just-rendered
//! tool result.

use std::collections::HashMap;

use super::injector::SteeringKind;

/// Per-turn occurrence tracker for read-only tool result `content_hash`
/// values. See module-level documentation for the contract.
///
/// The tracker only stores hashes; rendering the nudge body is
/// delegated to [`super::messages::render`] via
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

/// Threshold at which a single `content_hash` triggers the nudge.
/// Public so the agent-loop wiring tests can reference the same
/// constant the production code uses.
pub const REPEATED_READ_THRESHOLD: usize = 3;

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
    /// [`REPEATED_READ_THRESHOLD`] boundary and queued a nudge for the
    /// *next* turn; returns `false` for every other call (including
    /// the 4th, 5th, … repeat in the same turn — those are absorbed
    /// silently so the model receives at most one nudge per
    /// `(turn, content_hash)` pair).
    pub fn record(&mut self, content_hash: &str) -> bool {
        if content_hash.is_empty() {
            return false;
        }
        let count = self.counts.entry(content_hash.to_string()).or_insert(0);
        *count += 1;
        if *count == REPEATED_READ_THRESHOLD {
            self.pending.push(content_hash.to_string());
            true
        } else {
            false
        }
    }

    /// Returns the number of nudges currently queued for the next
    /// turn. Used by tests to assert that a 4th-and-later repeat in
    /// the same turn does not enqueue an extra nudge.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Begin a new model turn: drain the queued nudges, render them as
    /// [`SteeringKind`] values, and clear the per-turn counts.
    ///
    /// Callers append the returned [`SteeringKind`]s to the live user
    /// message via [`super::injector::SteeringInjector::inject`] so
    /// they ride into the prompt prefix the next model request reads.
    pub fn begin_turn(&mut self) -> Vec<SteeringKind> {
        self.counts.clear();
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|hash| SteeringKind::RepeatedRead { content_hash: hash })
            .collect()
    }
}
