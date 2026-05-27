//! [`SteeringKind`] enum + [`SteeringRenderer`] entry points.
//!
//! Wrapping every steering body in
//! `<harness_steering kind="...">...</harness_steering>` is the
//! contract Phase 2 inherits from PR D. The model treats the
//! envelope as a stable marker that "the harness — not the user, not
//! a tool result from the agent's own action — is telling me
//! something", so the kind label is the primary signal callers
//! should use to decide how to respond.

use super::messages;

/// Tagged union of per-iteration steering kinds the harness emits.
///
/// Variants are flat so call sites read as
/// `SteeringKind::TaskDoneNoWrites` rather than
/// `SteeringKind::TaskDoneRejected(TaskDoneReject::NoFileWrites)`.
/// Multiple variants share a single `kind="..."` label so the model
/// can pattern-match on the originating subsystem.
#[derive(Debug)]
pub enum SteeringKind {
    /// `task_done` rejected because no write/edit/delete tool calls
    /// were tracked and the agent did not set `no_changes_needed`.
    TaskDoneNoWrites,
    /// Post-write stub detector found one or more incomplete
    /// implementations and is asking the agent to fill them in
    /// before re-calling `task_done`.
    StubDetected {
        /// Plain-data view of the detected stubs, populated by the
        /// agent-side `file_ops::stub_detection` module.
        reports: Vec<StubReportView>,
    },
    /// Phase 3b: a single read-only tool result `content_hash` has
    /// been observed three or more times in one model turn. The
    /// nudge fires on the *next* turn (so it lands in the prompt
    /// prefix the model actually reads) and at most once per
    /// `(turn, content_hash)` pair.
    RepeatedRead {
        /// The `content_hash` value the read tool stamped on its
        /// metadata. The renderer truncates this to a short prefix
        /// when embedding it in the prose so the message stays
        /// readable.
        content_hash: String,
    },
    /// Phase 3a (minimum-viable): the executor observed the first
    /// read-only batch close on a task that declared a `test_command`,
    /// and is steering the model to verify whether the gate already
    /// passes before editing implementation files.
    TaskAlreadySatisfiedHint {
        /// The project's declared test command, copied verbatim into
        /// the rendered prose so the model sees the exact string it
        /// should invoke (or let the existing `task_done` DoD gate
        /// invoke).
        test_command: String,
    },
    /// Dev-loop read spiral: enough exploration tools ran with no
    /// cumulative file writes. Steers the model to mutate files on
    /// the next tool batch without ending the turn or blocking
    /// reads.
    ImplementNow {
        /// Number of exploration tools observed when the gate fired.
        exploration_count: usize,
        /// Sample of session-read paths to surface in the rendered
        /// body; capped to
        /// [`aura_config::IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE`] entries
        /// by the caller.
        sample_paths: Vec<String>,
    },
}

/// Plain-data view of one stub-detection hit threaded into
/// [`SteeringKind::StubDetected`].
///
/// The agent-side `crate::file_ops::stub_detection::StubReport`
/// carries an enum (`StubPattern`); this view flattens it to the
/// already-formatted display string so the prompt layer never has
/// to depend on `aura-agent` types.
#[derive(Debug, Clone)]
pub struct StubReportView {
    /// Workspace-relative path of the file containing the stub.
    pub path: String,
    /// 1-based source line of the stub.
    pub line: usize,
    /// Pre-formatted display of the [`StubPattern`] enum
    /// (e.g. `"todo!() macro"`).
    pub pattern: String,
    /// Source-line context the detector captured.
    pub context: String,
}

impl SteeringKind {
    /// Stable `kind="..."` label rendered into the envelope. Labels
    /// are grouped by subsystem so a single subsystem-level handler
    /// downstream can branch on the label without having to
    /// enumerate every sub-variant.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::TaskDoneNoWrites => "task_done_rejected",
            Self::StubDetected { .. } => "stub_detected",
            Self::RepeatedRead { .. } => "repeated_read",
            Self::TaskAlreadySatisfiedHint { .. } => "task_already_satisfied",
            Self::ImplementNow { .. } => "implement_now",
        }
    }
}

/// Stateless renderer that wraps a [`SteeringKind`] body in the
/// canonical `<harness_steering kind="…">…</harness_steering>`
/// envelope.
///
/// This used to be `SteeringInjector` (with both a `render` AND an
/// `inject` method that mutated `Vec<aura_reasoner::Message>`); the
/// `inject` half is now an agent-owned helper at
/// `aura-agent/src/agent_loop/steering/inject.rs` so this crate
/// stays free of the reasoner dep. Phase 5 will wire the steering
/// registry through that helper.
pub struct SteeringRenderer;

impl SteeringRenderer {
    /// Render `kind` to the canonical envelope and return the wrapped
    /// string without touching any message list. Used by every call
    /// site in `aura-agent`: tool-result rejections in
    /// `task_executor::handlers` (returned via `gate_rejection`) and
    /// the per-iteration `state.messages` injection in
    /// `agent_loop::steering::inject` (which appends the wrapped
    /// string to the user-message stream).
    #[must_use]
    pub fn render(kind: &SteeringKind) -> String {
        let body = messages::render(kind);
        let label = kind.label();
        format!("<harness_steering kind=\"{label}\">\n{body}\n</harness_steering>")
    }
}
