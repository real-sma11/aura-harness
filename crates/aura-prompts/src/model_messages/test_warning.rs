//! Post-`task_done` test-suite warning lines emitted by the executor
//! event channel.

/// Status line preceding the test-suite invocation.
#[must_use]
pub fn post_task_done_starting_line(cmd: &str, source: &str) -> String {
    format!("\n[post-task_done test run: {cmd} (source: {source})]\n")
}

/// Status line emitted when the post-`task_done` test run passes.
#[must_use]
pub fn post_task_done_passed_line(duration_ms: u64, summary: &str) -> String {
    format!("\n[post-task_done test run: PASSED in {duration_ms}ms — {summary}]\n")
}

/// Status line emitted when the post-`task_done` test run fails.
/// The Codex parity (May 2026) made this a warning, not a gate.
#[must_use]
pub fn post_task_done_failed_line(summary: &str) -> String {
    format!(
        "\n[post-task_done test run: FAILED — {summary}; \
         task_done still succeeded (suite is a warning, not a gate)]\n"
    )
}
