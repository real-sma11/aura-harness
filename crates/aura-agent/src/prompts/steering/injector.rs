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
/// originating subsystem (`task_done_rejected`, `stub_detected`) —
/// multiple variants share a single label so the model can
/// pattern-match on the subsystem rather than the specific sub-reason.
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
    /// Post-write stub detector found one or more incomplete
    /// implementations and is asking the agent to fill them in
    /// before re-calling `task_done`.
    StubDetected { reports: Vec<StubReport> },
    /// Phase 3b: a single read-only tool result `content_hash` has
    /// been observed three or more times in one model turn. The
    /// nudge fires on the *next* turn (so it lands in the prompt
    /// prefix the model actually reads) and at most once per
    /// `(turn, content_hash)` pair. Wording lives in
    /// [`super::messages::repeated_read`].
    RepeatedRead {
        /// The `content_hash` value the read tool stamped on its
        /// metadata (see `aura_tools::fs_tools::read::content_hash_hex`).
        /// The renderer truncates this to a short prefix when
        /// embedding it in the prose so the message stays readable.
        content_hash: String,
    },
    /// Phase 3a (minimum-viable): the executor observed the first
    /// read-only batch close on a task that declared a `test_command`,
    /// and is steering the model to verify whether the gate already
    /// passes before editing implementation files. Wording lives in
    /// [`super::messages::task_already_satisfied_hint`].
    ///
    /// The full oracle (run the test command, surface the actual
    /// `task_already_satisfied { summary }` message on a passing
    /// exit code) is the documented follow-up; this variant is the
    /// hint-only stand-in described in
    /// [`super::early_oracle`].
    TaskAlreadySatisfiedHint {
        /// The project's declared test command, copied verbatim into
        /// the rendered prose so the model sees the exact string it
        /// should invoke (or let the existing `task_done` DoD gate
        /// invoke).
        test_command: String,
    },
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
