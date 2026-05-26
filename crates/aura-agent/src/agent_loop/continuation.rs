//! Phase 1.B: goal-style continuation runtime.
//!
//! Aura's analog of codex's `maybe_start_goal_continuation_turn`
//! ([codex-rs/core/src/goals.rs:1289-1357](https://github.com/.../codex-rs/core/src/goals.rs))
//! plus the matching `continuation.md` template
//! ([codex-rs/core/templates/goals/continuation.md](https://github.com/.../codex-rs/core/templates/goals/continuation.md)).
//!
//! When a dev-loop iteration ends without a `write_file` / `edit_file`
//! / `delete_file` and the task is not yet complete, the runtime
//! injects a "continuation" prompt that pushes the agent toward
//! forward motion instead of letting it spiral on read-only tool
//! calls. After three consecutive no-write iterations the prompt
//! escalates to the codex "blocked" audit threshold; if the agent
//! still refuses to write, the loop is failed with `task_blocked`
//! after `max_continuation_turns` (default 6) injected continuations.
//!
//! The decision data comes from [`super::turn_diff::TurnDiff`]
//! (Phase 1.A), which already records per-path net file ops for the
//! current iteration.
//!
//! TODO(phase-1b): the optional integration test in `agent_loop/tests.rs`
//! that wires a synthetic dev-loop with a read-only mock model and
//! asserts the harness injects N nudges + Blocked + emits the
//! `task_blocked` exit is deferred — the existing mock-model
//! infrastructure isn't trivially adaptable, and the unit tests below
//! plus the post-iteration wiring in `mod.rs` already exercise the
//! transition table. Add when the test infra grows a "mock model that
//! returns text but no tool calls" affordance.

use super::turn_diff::TurnDiff;
use std::collections::HashSet;
use std::path::PathBuf;

/// Per-task state tracking how many consecutive iterations have ended
/// without a write. Persists across the iteration loop on `LoopState`.
#[derive(Debug, Default)]
pub(crate) struct ContinuationState {
    /// Number of consecutive no-write iterations observed so far.
    /// Reset to zero the moment a write lands.
    pub(crate) consecutive_no_write: u32,
    /// Last (up to 3) read-path sets from no-write iterations.
    /// Reserved for the blocker_signature audit that codex's
    /// `continuation.md` references — Phase 1.B uses the raw counter,
    /// but plumbing the read-path sets keeps the door open for the
    /// codex-style "are you reading the same files over and over?"
    /// signal in a follow-up.
    #[allow(dead_code)]
    pub(crate) recent_no_write_paths: Vec<HashSet<PathBuf>>,
}

/// What kind of continuation prompt the runtime should inject this
/// turn (or `None` from the producer when the iteration produced a
/// write or the task is complete).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContinuationKind {
    /// Iterations 1–2 without a write. Soft push: "either emit your
    /// next edit now, or call task_done with no_changes_needed".
    Nudge,
    /// Iteration ≥3 without a write. Codex's "blocked-after-3" audit
    /// threshold. The harness will fail the task on the next no-write
    /// iteration once `max_continuation_turns` is exhausted.
    Blocked,
}

impl ContinuationState {
    /// Update the streak counter based on whether the iteration that
    /// just ended produced a write. Returns `None` on a write (and
    /// resets state), `Some(Nudge)` for the first two consecutive
    /// no-write iterations, `Some(Blocked)` from the third onward.
    pub(crate) fn on_iteration_end(
        &mut self,
        diff: &TurnDiff,
        read_paths: HashSet<PathBuf>,
    ) -> Option<ContinuationKind> {
        if !diff.is_empty() {
            self.consecutive_no_write = 0;
            self.recent_no_write_paths.clear();
            return None;
        }
        self.consecutive_no_write = self.consecutive_no_write.saturating_add(1);
        self.recent_no_write_paths.push(read_paths);
        // Keep the window bounded to the last 3 no-write iterations.
        // Codex's audit uses the same triplet window.
        while self.recent_no_write_paths.len() > 3 {
            self.recent_no_write_paths.remove(0);
        }
        if self.consecutive_no_write >= 3 {
            Some(ContinuationKind::Blocked)
        } else {
            Some(ContinuationKind::Nudge)
        }
    }
}

/// Render the continuation envelope injected into the message history
/// before the next iteration. The body mirrors the codex template
/// (`templates/goals/continuation.md`) shape — XML-tagged so the
/// model can distinguish harness steering from user input.
pub(crate) fn render(kind: ContinuationKind, iter: usize, count: u32) -> String {
    match kind {
        ContinuationKind::Nudge => format!(
            "<harness_continuation kind=\"nudge\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. No write_file / edit_file / delete_file ran this turn. The dev loop is not yet complete.\n\
             \n\
             Either emit your next edit now, or call `task_done` with `no_changes_needed: true` and `notes` explaining why no change is needed. The task description may be stale - if the codebase contradicts the spec, trust the codebase.\n\
             </harness_continuation>"
        ),
        ContinuationKind::Blocked => format!(
            "<harness_continuation kind=\"blocked\" iteration=\"{iter}\" consecutive_no_write=\"{count}\">\n\
             Iteration {iter}. Three consecutive iterations without a write. This is the codex-style \"blocked\" audit threshold.\n\
             \n\
             You must do one of the following on the next turn:\n\
             1. Emit your best-effort write_file / edit_file / delete_file against the existing codebase.\n\
             2. Call `task_done` with `no_changes_needed: true` and `notes` describing the contradiction between the task description and the codebase.\n\
             \n\
             If you continue reading without one of the above, the harness will fail the task with `task_blocked`.\n\
             </harness_continuation>"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn empty_diff() -> TurnDiff {
        TurnDiff::default()
    }

    fn write_diff() -> TurnDiff {
        let mut d = TurnDiff::default();
        d.record_modify(PathBuf::from("src/lib.rs"), 10);
        d
    }

    #[test]
    fn continuation_resets_on_write() {
        let mut state = ContinuationState::default();
        let outcome = state.on_iteration_end(&write_diff(), HashSet::new());
        assert!(outcome.is_none());
        assert_eq!(state.consecutive_no_write, 0);
        assert!(state.recent_no_write_paths.is_empty());
    }

    #[test]
    fn continuation_nudge_on_iterations_1_and_2() {
        let mut state = ContinuationState::default();
        assert_eq!(
            state.on_iteration_end(&empty_diff(), HashSet::new()),
            Some(ContinuationKind::Nudge)
        );
        assert_eq!(state.consecutive_no_write, 1);
        assert_eq!(
            state.on_iteration_end(&empty_diff(), HashSet::new()),
            Some(ContinuationKind::Nudge)
        );
        assert_eq!(state.consecutive_no_write, 2);
    }

    #[test]
    fn continuation_blocked_on_iteration_3() {
        let mut state = ContinuationState::default();
        state.on_iteration_end(&empty_diff(), HashSet::new());
        state.on_iteration_end(&empty_diff(), HashSet::new());
        assert_eq!(
            state.on_iteration_end(&empty_diff(), HashSet::new()),
            Some(ContinuationKind::Blocked)
        );
        assert_eq!(state.consecutive_no_write, 3);
    }

    #[test]
    fn continuation_blocked_persists_until_write() {
        let mut state = ContinuationState::default();
        for _ in 0..5 {
            state.on_iteration_end(&empty_diff(), HashSet::new());
        }
        assert_eq!(state.consecutive_no_write, 5);
        // 4th and 5th no-write iterations remain Blocked.
        assert_eq!(
            state.on_iteration_end(&empty_diff(), HashSet::new()),
            Some(ContinuationKind::Blocked)
        );
        // A write resets the streak.
        assert_eq!(
            state.on_iteration_end(&write_diff(), HashSet::new()),
            None
        );
        assert_eq!(state.consecutive_no_write, 0);
        // The very next no-write iteration is back to a soft Nudge.
        assert_eq!(
            state.on_iteration_end(&empty_diff(), HashSet::new()),
            Some(ContinuationKind::Nudge)
        );
    }

    #[test]
    fn continuation_read_path_window_bounded_to_three() {
        let mut state = ContinuationState::default();
        for i in 0..10 {
            let mut paths = HashSet::new();
            paths.insert(PathBuf::from(format!("src/file_{i}.rs")));
            state.on_iteration_end(&empty_diff(), paths);
        }
        assert!(state.recent_no_write_paths.len() <= 3);
    }

    #[test]
    fn render_nudge_includes_envelope_and_iteration() {
        let body = render(ContinuationKind::Nudge, 4, 1);
        assert!(body.contains("<harness_continuation kind=\"nudge\""));
        assert!(body.contains("iteration=\"4\""));
        assert!(body.contains("consecutive_no_write=\"1\""));
        assert!(body.contains("trust the codebase"));
        assert!(body.contains("</harness_continuation>"));
    }

    #[test]
    fn render_blocked_includes_envelope_and_options() {
        let body = render(ContinuationKind::Blocked, 7, 3);
        assert!(body.contains("<harness_continuation kind=\"blocked\""));
        assert!(body.contains("iteration=\"7\""));
        assert!(body.contains("consecutive_no_write=\"3\""));
        assert!(body.contains("task_blocked"));
        assert!(body.contains("</harness_continuation>"));
    }
}
