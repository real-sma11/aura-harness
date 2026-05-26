//! Verbatim renderers for every [`SteeringKind`] body.
//!
//! Each function returns the inner steering text WITHOUT the surrounding
//! `<harness_steering>` wrapper — wrapping is the injector's job (see
//! [`super::injector::SteeringInjector::render`]). Wording is preserved
//! bit-for-bit from the pre-PR-D inline call sites in `task_executor`.
//!
//! New variants land here as new private renderers; the dispatcher
//! [`render`] picks one per [`SteeringKind`] arm.

use super::SteeringKind;
use crate::file_ops::StubReport;
use crate::prompts::fix::build_stub_fix_prompt;

/// Dispatcher used by [`super::injector::SteeringInjector`]. Returns the
/// unwrapped body for `kind`; the injector layers the envelope on top.
#[must_use]
pub(super) fn render(kind: &SteeringKind) -> String {
    match kind {
        SteeringKind::TaskDoneNoWrites => task_done_no_writes(),
        SteeringKind::StubDetected { reports } => stub_detected(reports),
        SteeringKind::RepeatedRead { content_hash } => repeated_read(content_hash),
        SteeringKind::TaskAlreadySatisfiedHint { test_command } => {
            task_already_satisfied_hint(test_command)
        }
    }
}

fn task_done_no_writes() -> String {
    "ERROR: task_done was rejected — you have not produced any file changes \
     (write_file / edit_file / delete_file). Implementation tasks must produce \
     file changes. Make the edits this task requires, then call task_done. \
     If this task genuinely requires no file changes, call task_done again with \
     \"no_changes_needed\": true and explain why in the \"notes\" field."
        .to_string()
}

fn stub_detected(reports: &[StubReport]) -> String {
    build_stub_fix_prompt(reports)
}

/// Number of leading hex chars from a `content_hash` we surface in the
/// repeated-read nudge. Short enough to keep the message readable, long
/// enough to be unique inside one turn (the read tool stamps a 16-hex
/// `u64` digest).
const REPEATED_READ_HASH_DISPLAY_CHARS: usize = 8;

fn repeated_read(content_hash: &str) -> String {
    let short: String = content_hash
        .chars()
        .take(REPEATED_READ_HASH_DISPLAY_CHARS)
        .collect();
    format!(
        "You've already read these exact bytes (content_hash={short}) 3 times this turn. \
         Use `start_line`/`end_line` to narrow the request, or move on — the file hasn't \
         changed."
    )
}

fn task_already_satisfied_hint(test_command: &str) -> String {
    format!(
        "task_already_satisfied {{\n  \
         test_command: \"{test_command}\",\n  \
         note: \"The harness has not run this command — it is hinting before any edits land. \
         If you have not yet verified the declared test gate, run `{test_command}` (or let \
         the `task_done` Definition-of-Done gate run it on completion) and inspect the \
         result before editing implementation files.\",\n  \
         hint: \"If the gate already passes, the task may already be satisfied. Consider \
         whether you should switch to test-augmentation mode and add coverage before changing \
         implementation.\"\n\
         }}"
    )
}
