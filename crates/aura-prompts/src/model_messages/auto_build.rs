//! Auto-build feedback strings emitted by the task executor on the
//! event channel ("UI prose"). These are not direct tool-result
//! contents but they are still model-facing — the dev-loop forwards
//! `TextDelta` events into the assistant's transcript via the
//! recording stream — so they live here for centralisation.

/// Render the prefix line displayed when an auto-build run begins.
#[must_use]
pub fn auto_build_status_line(cmd: &str) -> String {
    format!("\n[auto-build: {cmd}]\n")
}

/// Render the prefix line displayed when the stub detector finds
/// stubs and will request a fix.
#[must_use]
pub fn stub_detection_status_line(count: usize, attempt: u32, max_attempts: u32) -> String {
    format!("\n[stub detection] found {count} stub(s), requesting fix (attempt {attempt}/{max_attempts})\n")
}
