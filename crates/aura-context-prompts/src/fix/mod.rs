//! Build-fix prompt builder.
//!
//! Phase 2 split: this module is **formatting only**. The agent-side
//! caller (`aura-agent/src/verify/runner.rs` and friends) computes
//! every analysis-derived field and passes them in via
//! [`BuildFixPromptData`]. This module never calls
//! `classify_build_errors`, `parse_error_references`, or
//! `file_ops::resolve_error_context` — those depend on
//! `aura-agent` internals (build error classifier, file-ops resolver)
//! and are out of scope for the prompts crate.

use std::fmt::Write;

use crate::bootstrap::header::render_shared_header;
use crate::descriptors::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};

/// Plain-data record describing one previous fix attempt that
/// failed. The `aura-agent` `verify::error_types::BuildFixAttemptRecord`
/// converts to this view for rendering.
#[derive(Debug, Clone)]
pub struct PriorFixAttempt {
    /// Free-form summary of the attempt's changes (file ops + notes).
    /// May be empty, in which case [`Self::files_changed`] is shown.
    pub changes_summary: String,
    /// Workspace-relative file paths the attempt touched. Used only
    /// when [`Self::changes_summary`] is empty.
    pub files_changed: Vec<String>,
    /// stderr captured from the failing build of the attempt.
    pub stderr: String,
}

/// Pre-computed analysis inputs the fix prompt splices into the body.
///
/// Every analysis step that used to live inline in
/// `aura-agent::prompts::fix` (`classify_build_errors`,
/// `parse_error_references`, `resolve_error_context`,
/// `resolve_error_source_files`, `detect_api_hallucination`) is now
/// the agent's job. The prompt simply renders what it is given.
#[derive(Debug, Clone, Default)]
pub struct BuildFixPromptData {
    /// `aura-agent::build::error_category_guidance` output. Empty
    /// string skips the guidance section.
    pub guidance: String,
    /// Pre-resolved error-context block (from
    /// `aura-agent::file_ops::resolve_error_context`). Empty string
    /// skips the section.
    pub resolved_context: String,
    /// Pre-resolved error-source-files block (from
    /// `aura-agent::file_ops::resolve_error_source_files`). Empty
    /// string skips the section.
    pub error_source_files: String,
    /// Number of "method not found" errors the agent-side
    /// `parse_error_references` extracted. The prompt renders the
    /// "you are calling 3+ methods that do not exist" warning when
    /// this is `> 3`.
    pub methods_not_found_count: usize,
}

/// Inputs to [`build_fix_prompt`].
pub struct BuildFixPromptParams<'a> {
    pub project: &'a ProjectInfo<'a>,
    pub spec: &'a SpecInfo<'a>,
    pub task: &'a TaskInfo<'a>,
    pub session: &'a SessionInfo<'a>,
    /// Prior implementation notes ("# Notes from Initial Implementation").
    pub prior_notes: &'a str,
    /// Snapshot of the current codebase files relevant to the task.
    pub codebase_snapshot: &'a str,
    /// Build command that failed (for the header).
    pub build_command: &'a str,
    /// Failing stderr (truncated to ~8000 chars by [`build_fix_prompt`]).
    pub stderr: &'a str,
    /// Failing stdout (truncated to ~4000 chars by [`build_fix_prompt`]).
    pub stdout: &'a str,
    /// History of failed fix attempts.
    pub prior_attempts: &'a [PriorFixAttempt],
    /// Pre-computed analysis output. See [`BuildFixPromptData`].
    pub analysis: &'a BuildFixPromptData,
}

/// Render the build-fix prompt using the prepared analysis data.
#[must_use]
pub fn build_fix_prompt(params: &BuildFixPromptParams<'_>) -> String {
    let mut prompt = String::new();
    prompt.push_str(&format_fix_header(
        params.project,
        params.spec,
        params.task,
        params.session,
        params.prior_notes,
        params.prior_attempts,
    ));
    prompt.push_str(&format_fix_body(
        params.build_command,
        params.stderr,
        params.stdout,
        params.analysis,
        params.codebase_snapshot,
    ));
    prompt
}

fn format_fix_header(
    project: &ProjectInfo<'_>,
    spec: &SpecInfo<'_>,
    task: &TaskInfo<'_>,
    session: &SessionInfo<'_>,
    prior_notes: &str,
    prior_attempts: &[PriorFixAttempt],
) -> String {
    let mut header = render_shared_header(project, spec, task, session);

    if !prior_notes.is_empty() {
        let _ = write!(
            header,
            "# Notes from Initial Implementation\n{prior_notes}\n\n",
        );
    }

    if !prior_attempts.is_empty() {
        header.push_str("# Previous Fix Attempts (all failed)\nThe following fixes were already attempted and did NOT solve the problem. You MUST try a fundamentally different approach.\n\n");
        for (i, attempt) in prior_attempts.iter().enumerate() {
            let _ = writeln!(header, "## Attempt {}", i + 1);
            if !attempt.changes_summary.is_empty() {
                let _ = write!(header, "Changes made:\n{}\n", attempt.changes_summary);
            } else if !attempt.files_changed.is_empty() {
                header.push_str("Files changed:\n");
                for f in &attempt.files_changed {
                    let _ = writeln!(header, "- {f}");
                }
            }
            let _ = write!(header, "Error:\n```\n{}\n```\n\n", attempt.stderr);
        }
    }

    header
}

fn format_fix_body(
    build_command: &str,
    stderr: &str,
    stdout: &str,
    analysis: &BuildFixPromptData,
    codebase_snapshot: &str,
) -> String {
    let mut body = String::new();

    let _ = write!(
        body,
        "# Build/Test Verification FAILED\n\
         The command `{build_command}` failed after the previous file operations were applied.\n\
         You MUST fix ALL errors below.\n\n",
    );

    if !analysis.guidance.is_empty() {
        let _ = write!(
            body,
            "## Error Analysis & Required Fix Strategy\n{}\n",
            analysis.guidance,
        );
    }

    let truncated_stderr = truncate_prompt_output(stderr, 8000);
    let _ = write!(body, "## stderr\n```\n{truncated_stderr}\n```\n\n");

    if !stdout.is_empty() {
        let truncated_stdout = truncate_prompt_output(stdout, 4000);
        let _ = write!(body, "## stdout\n```\n{truncated_stdout}\n```\n\n");
    }

    if analysis.methods_not_found_count > 3 {
        body.push_str(
            "WARNING: You are calling 3+ methods that do not exist. You MUST use ONLY \
             the methods listed in the \"Actual API Reference\" section below. Do NOT \
             invent or guess method names.\n\n",
        );
    }

    if !analysis.resolved_context.is_empty() {
        body.push_str(&analysis.resolved_context);
        body.push('\n');
    }

    if !analysis.error_source_files.is_empty() {
        body.push_str(&analysis.error_source_files);
        body.push('\n');
    }

    if !codebase_snapshot.is_empty() {
        let _ = write!(
            body,
            "# Current Codebase Files (after previous changes)\n{codebase_snapshot}\n",
        );
    }

    body
}

fn truncate_prompt_output(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let half = max_chars / 2;
    let start = &s[..half];
    let end = &s[s.len() - half..];
    format!(
        "{start}\n\n... (truncated {0} bytes) ...\n\n{end}",
        s.len() - max_chars
    )
}

#[cfg(test)]
mod tests;
