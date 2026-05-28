//! `task_done` tool-result rejection bodies.
//!
//! Some `task_done` rejections are wrapped by the
//! [`crate::steering::SteeringRenderer`] envelope (`TaskDoneNoWrites`,
//! `StubDetected`); these here are the inline rejections the
//! `task_executor` issues directly via `gate_rejection`. They live
//! together so the wording is auditable in one place.

/// Body emitted when an exploration tool is attempted in the
/// `task_done was rejected because no file changes were produced`
/// short-circuit window — the executor blocks the next read/search
/// tool until the agent issues a write or re-calls `task_done` with
/// `no_changes_needed: true`.
pub const NO_WRITES_AFTER_REJECT_BODY: &str =
    "task_done was just rejected because no file changes were produced. Your next action must be \
     write_file / edit_file / delete_file, or task_done with no_changes_needed: true and notes \
     explaining why the task is already satisfied.";

/// Body emitted by the pervasive-errors gate when the most recent
/// `run_command` exited non-zero.
pub const LAST_COMMAND_FAILED_BODY: &str = "ERROR: The last run_command exited non-zero. \
     Your build or test is broken. Fix the errors before completing the task. \
     (Policy-denied commands do not count — if run_command is blocked, rely on \
     the harness's auto-build step and do not keep calling run_command.)";

/// Render the pervasive-errors body emitted when a high fraction of
/// recent tool calls are returning errors.
#[must_use]
pub fn pervasive_errors_body(real_errors: usize, total: usize, error_ratio: f64) -> String {
    format!(
        "ERROR: {real_errors}/{total} recent tool calls returned errors \
         ({:.0}% failure rate, policy denials excluded). The task is likely \
         incomplete. Review the errors, fix the underlying issue, then try \
         completing again.",
        error_ratio * 100.0,
    )
}

/// Render the self-review prompt issued when the executor detects
/// the agent has not re-read the files it modified before calling
/// `task_done`.
#[must_use]
pub fn self_review_required_body(unreviewed_paths: &[String]) -> String {
    format!(
        "SELF-REVIEW REQUIRED: Before completing, re-read the files you modified \
         to verify correctness:\n{}\n\nCheck: (a) changes match task requirements, \
         (b) no placeholder/stub code remains, (c) no debug code left behind.\n\
         Then call task_done again.",
        unreviewed_paths.join("\n"),
    )
}

/// JSON body returned to the model when `task_done` is accepted.
pub const TASK_DONE_COMPLETED_JSON: &str = r#"{"status":"completed"}"#;

/// Render the `submit_plan` acceptance body. The plan itself is
/// formatted by the caller and passed in as `context_string`.
#[must_use]
pub fn submit_plan_accepted_body(context_string: &str) -> String {
    format!(
        "Plan recorded for reference. Implementation can already \
         proceed — writes (write_file/edit_file/delete_file) and \
         task_done are accepted regardless of whether submit_plan \
         was called. This call reset the rolling-outcome window.\n\n\
         YOUR PLAN (reference during implementation):\n{context_string}\n\n\
         Continue with the most foundational changes first.",
    )
}

/// Render the `submit_plan` rejection body when the plan validator
/// returns an error.
#[must_use]
pub fn submit_plan_rejected_body(reason: &str) -> String {
    format!("Plan rejected: {reason}. Revise and resubmit.")
}
