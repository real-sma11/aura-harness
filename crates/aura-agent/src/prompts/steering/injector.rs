//! [`SteeringKind`] enum + [`SteeringInjector`] entry points.
//!
//! Wrapping every steering body in
//! `<harness_steering kind="...">...</harness_steering>` is the
//! contract PR D establishes. The model treats the envelope as a
//! stable marker that "the harness — not the user, not a tool result
//! from the agent's own action — is telling me something", so the
//! kind label is the primary signal callers should use to decide how
//! to respond.

use aura_reasoner::Message;

use super::messages;
use crate::file_ops::StubReport;
use crate::helpers;

/// Tagged union of per-iteration steering kinds the harness emits.
///
/// Variants are flat so call sites read as
/// `SteeringKind::TaskDoneNoWrites` rather than
/// `SteeringKind::TaskDoneRejected(TaskDoneReject::NoFileWrites)`,
/// which is the live shape `task_executor::handlers` consumes. The
/// label rendered into the `kind="..."` attribute is grouped by the
/// originating subsystem (`task_done_rejected`,
/// `apply_patch_parse_error`, `stub_detected`) — multiple variants
/// share a single label so the model can pattern-match on the
/// subsystem rather than the specific sub-reason.
///
/// Variants only exist for call sites the audit (PR D Step 1)
/// confirmed are still live; the original PR D plan also listed
/// `EndturnWithoutWrite`, `ReadOnlyStreak`, and `NarrationBudget`,
/// but those injection paths were already deleted by concurrent
/// commits (`955d05b`, `91be64f`, `91e6f7d`) before PR D landed, so
/// they are intentionally absent here.
#[derive(Debug)]
pub enum SteeringKind {
    /// `task_done` rejected because no write/edit/delete tool calls
    /// were tracked and the agent did not set `no_changes_needed`.
    TaskDoneNoWrites,
    /// `task_done` rejected because the Definition-of-Done test gate
    /// ran the configured test command and the suite reported
    /// failures. `failures_block` and `stderr_block` are
    /// pre-rendered (already trimmed/truncated by the caller) so the
    /// renderer is purely textual.
    TaskDoneTestGateFailed {
        cmd: String,
        attempt: usize,
        max_attempts: usize,
        summary: String,
        failures_block: String,
        stderr_block: String,
    },
    /// `task_done` rejected and the DoD test gate retry budget is
    /// exhausted. The renderer appends a "retry budget is exhausted"
    /// footer to the [`Self::TaskDoneTestGateFailed`] body.
    TaskDoneTestGateExhausted {
        cmd: String,
        attempt: usize,
        max_attempts: usize,
        summary: String,
        failures_block: String,
        stderr_block: String,
    },
    /// The DoD test runner itself failed to execute the configured
    /// test command (spawn error, command not found, etc.).
    TaskDoneTestGateIoFailure {
        cmd: String,
        error: String,
        attempt: usize,
        max_attempts: usize,
    },
    /// `apply_patch` was invoked without a `patch` argument.
    ApplyPatchMissingArgument,
    /// `apply_patch` could not parse the model's envelope.
    ApplyPatchParseFailed { err: String },
    /// `*** Add File:` named a path that already exists.
    ApplyPatchTargetAlreadyExists { path: String },
    /// Update/Delete named a path that does not exist.
    ApplyPatchTargetNotFound { path: String },
    /// Patch path tried to escape the workspace root.
    ApplyPatchPathEscape { path: String },
    /// A hunk's context lines did not match the file. `hunk_index`
    /// is 0-based; the renderer adds 1 so the operator sees
    /// `hunk #1`.
    ApplyPatchContextMismatch {
        path: String,
        hunk_index: usize,
        reason: String,
    },
    /// Two directives in one patch envelope touched the same path
    /// in conflicting ways.
    ApplyPatchConflictingChanges { path: String, reason: String },
    /// Filesystem error during apply.
    ApplyPatchIo { path: String, source: String },
    /// Post-write stub detector found one or more incomplete
    /// implementations and is asking the agent to fill them in
    /// before re-calling `task_done`.
    StubDetected { reports: Vec<StubReport> },
}

impl SteeringKind {
    /// Stable `kind="..."` label rendered into the envelope. Labels
    /// are grouped by subsystem so a single subsystem-level handler
    /// downstream can branch on the label without having to
    /// enumerate every sub-variant.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::TaskDoneNoWrites
            | Self::TaskDoneTestGateFailed { .. }
            | Self::TaskDoneTestGateExhausted { .. }
            | Self::TaskDoneTestGateIoFailure { .. } => "task_done_rejected",
            Self::ApplyPatchMissingArgument
            | Self::ApplyPatchParseFailed { .. }
            | Self::ApplyPatchTargetAlreadyExists { .. }
            | Self::ApplyPatchTargetNotFound { .. }
            | Self::ApplyPatchPathEscape { .. }
            | Self::ApplyPatchContextMismatch { .. }
            | Self::ApplyPatchConflictingChanges { .. }
            | Self::ApplyPatchIo { .. } => "apply_patch_parse_error",
            Self::StubDetected { .. } => "stub_detected",
        }
    }
}

/// Entry points for routing every per-iteration model-facing string
/// through the canonical [`SteeringKind`] envelope.
///
/// This is a stateless namespace type, not a value. It exists so
/// downstream call sites read as `SteeringInjector::inject(...)` /
/// `SteeringInjector::render(...)` which makes the contract obvious
/// at the call site.
pub struct SteeringInjector;

impl SteeringInjector {
    /// Render `kind` to the canonical envelope and return the wrapped
    /// string without touching any message list. Used by call sites
    /// that route the steering text back to the model through the
    /// tool-result channel (e.g. `task_executor::handlers` returning
    /// a rejection body via `gate_rejection`) rather than appending
    /// to `state.messages`.
    #[must_use]
    pub fn render(kind: &SteeringKind) -> String {
        let body = messages::render(kind);
        let label = kind.label();
        format!("<harness_steering kind=\"{label}\">\n{body}\n</harness_steering>")
    }

    /// Render `kind` and append the wrapped body to the live
    /// user-message stream via [`helpers::append_warning`]. Returns
    /// the wrapped string so callers can also emit it on a stream
    /// channel.
    ///
    /// PR D leaves no live call sites of this method — every
    /// enumerated steering kind that previously used `append_warning`
    /// (`build_progress_demand`, `narration_steering_message`, the
    /// read-only-streak `STOP READING` nudge) was removed before PR
    /// D landed. The method stays on the surface so future
    /// per-iteration injection kinds have an obvious entry point and
    /// the contract ("every steering message wears the envelope")
    /// does not regress.
    pub fn inject(messages: &mut Vec<Message>, kind: SteeringKind) -> String {
        let wrapped = Self::render(&kind);
        helpers::append_warning(messages, &wrapped);
        wrapped
    }
}
