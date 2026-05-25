//! Verbatim renderers for every [`SteeringKind`] body.
//!
//! Each function returns the inner steering text WITHOUT the surrounding
//! `<harness_steering>` wrapper — wrapping is the injector's job (see
//! [`super::injector::SteeringInjector::render`]). Wording is preserved
//! bit-for-bit from the pre-PR-D inline call sites in `task_executor`
//! so the only behaviour change PR D ships is the envelope itself.
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
        SteeringKind::TaskDoneTestGateFailed {
            cmd,
            attempt,
            max_attempts,
            summary,
            failures_block,
            stderr_block,
        } => task_done_test_gate_failed(
            cmd,
            *attempt,
            *max_attempts,
            summary,
            failures_block,
            stderr_block,
        ),
        SteeringKind::TaskDoneTestGateExhausted {
            cmd,
            attempt,
            max_attempts,
            summary,
            failures_block,
            stderr_block,
        } => task_done_test_gate_exhausted(
            cmd,
            *attempt,
            *max_attempts,
            summary,
            failures_block,
            stderr_block,
        ),
        SteeringKind::TaskDoneTestGateIoFailure {
            cmd,
            error,
            attempt,
            max_attempts,
        } => task_done_test_gate_io_failure(cmd, error, *attempt, *max_attempts),
        SteeringKind::ApplyPatchMissingArgument => apply_patch_missing_argument(),
        SteeringKind::ApplyPatchParseFailed { err } => apply_patch_parse_failed(err),
        SteeringKind::ApplyPatchTargetAlreadyExists { path } => {
            apply_patch_target_already_exists(path)
        }
        SteeringKind::ApplyPatchTargetNotFound { path } => apply_patch_target_not_found(path),
        SteeringKind::ApplyPatchPathEscape { path } => apply_patch_path_escape(path),
        SteeringKind::ApplyPatchContextMismatch {
            path,
            hunk_index,
            reason,
        } => apply_patch_context_mismatch(path, *hunk_index, reason),
        SteeringKind::ApplyPatchConflictingChanges { path, reason } => {
            apply_patch_conflicting_changes(path, reason)
        }
        SteeringKind::ApplyPatchIo { path, source } => apply_patch_io(path, source),
        SteeringKind::StubDetected { reports } => stub_detected(reports),
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

fn task_done_test_gate_failed(
    cmd: &str,
    attempt: usize,
    max_attempts: usize,
    summary: &str,
    failures_block: &str,
    stderr_block: &str,
) -> String {
    let header = format!(
        "ERROR: task_done blocked by Definition-of-Done test gate. \
         Running `{cmd}` reported failures (gate attempt {attempt}/{max_attempts}). \
         Fix EVERY failing test in the project — including tests that were already broken before \
         your task — then call task_done again.\n\nSummary: {summary}",
    );
    format!("{header}{failures_block}{stderr_block}")
}

fn task_done_test_gate_exhausted(
    cmd: &str,
    attempt: usize,
    max_attempts: usize,
    summary: &str,
    failures_block: &str,
    stderr_block: &str,
) -> String {
    let prompt = task_done_test_gate_failed(
        cmd,
        attempt,
        max_attempts,
        summary,
        failures_block,
        stderr_block,
    );
    format!(
        "{prompt}\n\nThis is attempt {attempt}/{max_attempts}. The \
         test gate retry budget is exhausted; the task is being marked as failed \
         with dod_test_gate_exhausted=true so the orchestrator can decide how to \
         proceed."
    )
}

fn task_done_test_gate_io_failure(
    cmd: &str,
    error: &str,
    attempt: usize,
    max_attempts: usize,
) -> String {
    format!(
        "ERROR: task_done test gate failed to execute `{cmd}`: {error}. \
         Fix the test command or your project setup, then call task_done \
         again. (gate attempt {attempt}/{max_attempts})"
    )
}

fn apply_patch_missing_argument() -> String {
    "apply_patch requires a non-empty `patch` string argument containing the \
     full `*** Begin Patch ... *** End Patch` envelope."
        .to_string()
}

fn apply_patch_parse_failed(err: &str) -> String {
    format!(
        "apply_patch failed to parse: {err}.\n\n\
         Re-emit the patch with a well-formed envelope:\n\
         *** Begin Patch\n\
         *** Add File: path/to/new.rs   (or *** Update File: / *** Delete File:)\n\
         +content lines (Add) / @@ context @@ then -removed / +added / `space`+context (Update)\n\
         *** End Patch"
    )
}

fn apply_patch_target_already_exists(path: &str) -> String {
    format!(
        "apply_patch error: `*** Add File: {path}` rejected because the target \
         already exists. Use `*** Update File:` for an existing file, or \
         `*** Delete File:` first if you really want to replace it."
    )
}

fn apply_patch_target_not_found(path: &str) -> String {
    format!(
        "apply_patch error: target file `{path}` does not exist. Read the project \
         to verify the path, or use `*** Add File:` if you intend to create it."
    )
}

fn apply_patch_path_escape(path: &str) -> String {
    format!(
        "apply_patch error: path `{path}` resolves outside the workspace root. All \
         patch paths must be workspace-relative (no `..`, no absolute / drive-letter \
         paths)."
    )
}

fn apply_patch_context_mismatch(path: &str, hunk_index: usize, reason: &str) -> String {
    let n = hunk_index + 1;
    format!(
        "apply_patch error: hunk #{n} in `{path}` did not match the file. {reason}\n\n\
         Read the target file with `read_file` and re-derive the hunk's context lines \
         from real bytes before re-emitting the patch.",
    )
}

fn apply_patch_conflicting_changes(path: &str, reason: &str) -> String {
    format!(
        "apply_patch error: conflicting directives for `{path}` within one patch. \
         {reason} Combine them into a single directive instead."
    )
}

fn apply_patch_io(path: &str, source: &str) -> String {
    format!("apply_patch error: filesystem failure on `{path}`: {source}")
}

fn stub_detected(reports: &[StubReport]) -> String {
    build_stub_fix_prompt(reports)
}
