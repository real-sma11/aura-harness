//! Shared header used by both [`super::build_agentic_task_context`]
//! and [`crate::fix::build_fix_prompt`].
//!
//! The fix-prompt path emits a slightly larger header (with prior-
//! attempt blocks) but the leading project/spec/task lines are
//! byte-identical with the bootstrap context. Centralising the lead
//! lets a future wording change land in exactly one place.

use std::fmt::Write;

use crate::descriptors::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};

/// Render the leading lines shared by the bootstrap and fix prompts:
///
/// ```text
/// # Project: <name>
/// <description>
///
/// # Spec: <title>
/// <markdown_contents>
///
/// # Task: <title>
/// <description>
///
/// # Previous Context Summary
/// <summary>
/// ```
///
/// `description` / `markdown_contents` blocks are emitted verbatim.
/// The spec body is **not** truncated here — that is the caller's
/// responsibility (the bootstrap path uses a byte budget; the fix
/// path keeps the spec verbatim).
#[must_use]
pub fn render_shared_header(
    project: &ProjectInfo<'_>,
    spec: &SpecInfo<'_>,
    task: &TaskInfo<'_>,
    session: &SessionInfo<'_>,
) -> String {
    let mut header = String::new();
    let _ = write!(
        header,
        "# Project: {}\n{}\n\n",
        project.name, project.description
    );
    let _ = write!(
        header,
        "# Spec: {}\n{}\n\n",
        spec.title, spec.markdown_contents
    );
    let _ = write!(header, "# Task: {}\n{}\n\n", task.title, task.description);
    if !session.summary_of_previous_context.is_empty() {
        let _ = write!(
            header,
            "# Previous Context Summary\n{}\n\n",
            session.summary_of_previous_context
        );
    }
    header
}
