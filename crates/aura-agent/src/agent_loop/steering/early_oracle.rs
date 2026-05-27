//! Early "is the test gate already green?" oracle (Phase 3a of the
//! reread-efficiency plan, minimum-viable variant).
//!
//! # Background
//!
//! The Shamir-recovery transcript that motivated Phase 3 watched the
//! agent re-read 8 source files 30+ times before noticing that the
//! declared test gate already passed in 1386 ms — the task was already
//! satisfied before the agent's first edit. The full oracle (run the
//! `test_command`, parse the exit code, surface a `task_already_satisfied`
//! system message when it passes) short-circuits that flow.
//!
//! # Why this is the minimum-viable variant, not the full oracle
//!
//! Wiring the full oracle into the live agent loop requires async
//! plumbing into the streaming sampling pump (E.4) at exactly the
//! "first read-only batch closed, but no write yet" boundary. That
//! boundary today does not exist as a single seam — the pump in
//! [`crate::agent_loop::stream_pump`] interleaves tool execution with
//! `OutputItemDone` events on a `FuturesOrdered`, and reaching back into
//! it to spawn an out-of-band `cargo test` invocation safely (with the
//! existing per-tool timeout, cancellation token, and event channel
//! semantics intact) is invasive enough to risk regressing the streaming
//! path's parity guarantees.
//!
//! Phase 3 therefore ships the oracle in two parts:
//!
//! 1. The state machine that decides *when* the hint should fire:
//!    [`EarlyTestOracle`]. Driven by the same `tool_name` strings the
//!    agent loop already sees on every `ToolCallInfo`, it tracks the
//!    "first read-only batch" boundary precisely.
//! 2. A [`SteeringKind::TaskAlreadySatisfiedHint`] variant that
//!    surfaces the hint without claiming the test gate has already
//!    been verified. The body matches the
//!    `task_already_satisfied { … }` shape the plan specified, except
//!    `summary` is replaced with a `note` that the harness has *not*
//!    run the test command — the model still has to invoke it (or
//!    let the existing `task_done` DoD gate run it on completion).
//!
//! Phase 2 of the core-loop architecture refactor relocated the
//! oracle out of `prompts/steering/` (where it had no business
//! living once the prompts layer became render-only) into
//! `agent_loop/steering/`. Phase 5 will wire the oracle into the
//! live loop; today the relocation is behaviour-preserving.

use aura_prompts::SteeringKind;

use crate::helpers::{is_exploration_tool, is_write_tool};

/// State machine tracking the "first read-only batch closed" boundary.
///
/// A *batch* is a contiguous run of read-only tool calls
/// (`read_file` / `list_files` / `search_code` / `stat_file` /
/// `find_files`); the batch closes the first time the agent issues a
/// non-read tool (typically a write, but any non-read counts) or the
/// caller explicitly signals a turn boundary via [`Self::close_batch`].
///
/// Once closed, the oracle queues exactly one
/// [`SteeringKind::TaskAlreadySatisfiedHint`] which subsequent calls
/// to [`Self::take_hint`] return — and only the *first* such call
/// returns it. Subsequent batches do not re-fire the hint; this is a
/// once-per-task signal by design.
//
// `OracleState`, [`EarlyTestOracle`], and its inherent methods are
// `#[allow(dead_code)]` because Phase 5 of the core-loop architecture
// refactor wires the oracle into `LoopState::steering`. Until that
// patch lands the type lives behind unit tests only; the lint
// suppression is the documented Phase 2 → Phase 5 hand-off.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OracleState {
    AwaitingFirstRead,
    InsideFirstBatch,
    HintQueued,
    Done,
}

/// Minimum-viable early test-gate oracle.
///
/// Construction parameters:
///
/// * `test_command` — the project's declared test command. Required
///   for the oracle to produce a hint; when `None`, the oracle is
///   permanently in `Done` and never fires.
/// * `enabled` — the per-task config knob (the
///   `early_test_oracle: bool` field on `TaskRunConfig`). Default is
///   `true` for `TaskRun` automatons; set to `false` for ad-hoc chat
///   sessions or any task where the operator wants the oracle off.
///
/// The oracle is single-shot: at most one hint per [`Self::take_hint`]
/// caller, regardless of how many batches the task subsequently opens.
#[allow(dead_code)]
#[derive(Debug)]
pub struct EarlyTestOracle {
    test_command: Option<String>,
    state: OracleState,
}

#[allow(dead_code)]
impl EarlyTestOracle {
    /// Construct an oracle bound to the task's declared test command.
    ///
    /// The oracle short-circuits to `Done` when `enabled` is `false`
    /// or `test_command` is `None`, so callers can construct one
    /// unconditionally and let the state machine gate itself.
    #[must_use]
    pub fn new(test_command: Option<String>, enabled: bool) -> Self {
        let state = if enabled
            && test_command
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
        {
            OracleState::AwaitingFirstRead
        } else {
            OracleState::Done
        };
        Self {
            test_command,
            state,
        }
    }

    /// Returns `true` when the oracle is still armed (i.e. has a
    /// queued or pending hint). Tests use this as a low-noise probe
    /// alternative to inspecting the [`OracleState`] enum directly.
    #[must_use]
    pub fn is_armed(&self) -> bool {
        !matches!(self.state, OracleState::Done)
    }

    /// Record a tool call by name. Read-only tool names extend the
    /// open batch; any other tool name closes it (queuing a hint if
    /// the batch was non-empty).
    pub fn observe_tool(&mut self, tool_name: &str) {
        match self.state {
            OracleState::AwaitingFirstRead => {
                if is_exploration_tool(tool_name) {
                    self.state = OracleState::InsideFirstBatch;
                }
            }
            OracleState::InsideFirstBatch => {
                if !is_exploration_tool(tool_name) {
                    self.state = OracleState::HintQueued;
                }
            }
            OracleState::HintQueued | OracleState::Done => {}
        }
        if matches!(self.state, OracleState::AwaitingFirstRead) && is_write_tool(tool_name) {
            self.state = OracleState::Done;
        }
    }

    /// Explicitly close the first read-only batch (e.g. on a model
    /// turn boundary where the next batch will start). Idempotent
    /// once the hint has been queued / taken.
    pub fn close_batch(&mut self) {
        if matches!(self.state, OracleState::InsideFirstBatch) {
            self.state = OracleState::HintQueued;
        }
    }

    /// Return the queued hint exactly once, if any. Subsequent calls
    /// always return `None` — the oracle is single-shot by design.
    pub fn take_hint(&mut self) -> Option<SteeringKind> {
        if !matches!(self.state, OracleState::HintQueued) {
            return None;
        }
        let test_command = self.test_command.clone()?;
        self.state = OracleState::Done;
        Some(SteeringKind::TaskAlreadySatisfiedHint { test_command })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_hint_after_first_read_only_batch_when_test_command_declared() {
        let mut oracle = EarlyTestOracle::new(Some("cargo --version".into()), true);

        assert!(
            oracle.is_armed(),
            "oracle must be armed when enabled and a test_command is declared"
        );
        assert!(
            oracle.take_hint().is_none(),
            "no hint should be emitted before any read-only batch is observed"
        );

        oracle.observe_tool("read_file");
        oracle.observe_tool("list_files");
        assert!(
            oracle.take_hint().is_none(),
            "no hint should be emitted while the first read-only batch is still open"
        );

        oracle.observe_tool("edit_file");

        let hint = oracle
            .take_hint()
            .expect("hint must be emitted when the first read-only batch closes");
        match hint {
            SteeringKind::TaskAlreadySatisfiedHint { test_command } => {
                assert_eq!(test_command, "cargo --version");
            }
            other => panic!("unexpected steering kind from oracle: {other:?}"),
        }

        assert!(
            !oracle.is_armed(),
            "oracle must disarm itself after firing exactly once"
        );
        assert!(
            oracle.take_hint().is_none(),
            "second take_hint call must return None — the oracle is single-shot"
        );
    }

    #[test]
    fn close_batch_explicit_boundary_fires_hint() {
        let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), true);
        oracle.observe_tool("read_file");
        oracle.observe_tool("read_file");
        oracle.close_batch();
        let hint = oracle
            .take_hint()
            .expect("explicit close_batch must queue the hint identically to a write boundary");
        assert!(matches!(
            hint,
            SteeringKind::TaskAlreadySatisfiedHint { .. }
        ));
    }

    #[test]
    fn disabled_never_fires() {
        let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), false);
        assert!(!oracle.is_armed());
        oracle.observe_tool("read_file");
        oracle.observe_tool("write_file");
        assert!(oracle.take_hint().is_none());
    }

    #[test]
    fn without_test_command_never_fires() {
        let mut oracle = EarlyTestOracle::new(None, true);
        assert!(!oracle.is_armed());
        oracle.observe_tool("read_file");
        oracle.observe_tool("write_file");
        assert!(oracle.take_hint().is_none());
    }

    #[test]
    fn blank_test_command_never_fires() {
        let oracle = EarlyTestOracle::new(Some("   ".into()), true);
        assert!(
            !oracle.is_armed(),
            "blank test_command must short-circuit to the disarmed state"
        );
    }

    #[test]
    fn write_first_disarms_without_firing() {
        let mut oracle = EarlyTestOracle::new(Some("cargo test".into()), true);
        oracle.observe_tool("write_file");
        assert!(oracle.take_hint().is_none());
        assert!(!oracle.is_armed());
    }
}
