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
//! The actual `cargo test` invocation is the remaining follow-up; the
//! state machine and the steering kind are complete and unit-tested
//! today so the wiring patch can land in a focused PR with no new
//! types or wording changes.

use super::injector::SteeringKind;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OracleState {
    /// No read-only tool has been observed yet.
    AwaitingFirstRead,
    /// At least one read-only tool has been observed; the batch is
    /// still open and will close on the next non-read tool call or
    /// explicit boundary.
    InsideFirstBatch,
    /// Batch boundary has been crossed; a hint is queued and ready
    /// for the next [`Self::take_hint`] call.
    HintQueued,
    /// Hint has already been taken (or the oracle was disabled / no
    /// `test_command` was declared) and will not fire again.
    Done,
}

/// Minimum-viable early test-gate oracle.
///
/// Construction parameters:
///
/// * `test_command` — the project's declared test command. Required
///   for the oracle to produce a hint; when `None`, the oracle is
///   permanently in [`OracleState::Done`] and never fires.
/// * `enabled` — the per-task config knob (the
///   `early_test_oracle: bool` field on `TaskRunConfig`). Default is
///   `true` for `TaskRun` automatons; set to `false` for ad-hoc chat
///   sessions or any task where the operator wants the oracle off.
///
/// The oracle is single-shot: at most one hint per [`Self::take_hint`]
/// caller, regardless of how many batches the task subsequently opens.
#[derive(Debug)]
pub struct EarlyTestOracle {
    test_command: Option<String>,
    state: OracleState,
}

impl EarlyTestOracle {
    /// Construct an oracle bound to the task's declared test command.
    ///
    /// The oracle short-circuits to [`OracleState::Done`] when
    /// `enabled` is `false` or `test_command` is `None`, so callers
    /// can construct one unconditionally and let the state machine
    /// gate itself.
    #[must_use]
    pub fn new(test_command: Option<String>, enabled: bool) -> Self {
        let state = if enabled && test_command.as_deref().is_some_and(|s| !s.trim().is_empty()) {
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
    ///
    /// Tool names that are neither read-only nor write — e.g. a
    /// `task_done` invocation — also close the batch: the agent has
    /// stepped out of the exploration phase, which is exactly the
    /// signal the oracle is watching for.
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
        // Defensive: even when we're "AwaitingFirstRead" and see a
        // write tool first (no exploration at all), the oracle's
        // job is finished — the agent is not in the read-heavy
        // pre-edit phase the hint targets.
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
