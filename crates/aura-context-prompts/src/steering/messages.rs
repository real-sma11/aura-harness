//! Verbatim renderers for every [`super::SteeringKind`] body.
//!
//! Each function returns the inner steering text WITHOUT the
//! surrounding `<harness_steering>` wrapper — wrapping is
//! [`super::SteeringRenderer::render`]'s job. Wording is preserved
//! bit-for-bit from the pre-Phase-2 inline call sites.
//!
//! New variants land here as new private renderers; the dispatcher
//! [`render`] picks one per [`SteeringKind`] arm.

use std::fmt::Write;

use super::kind::{SteeringKind, StubReportView};

/// Dispatcher used by [`super::SteeringRenderer`]. Returns the
/// unwrapped body for `kind`; the renderer layers the envelope on
/// top.
#[must_use]
pub(super) fn render(kind: &SteeringKind) -> String {
    match kind {
        SteeringKind::TaskDoneNoWrites => task_done_no_writes(),
        SteeringKind::StubDetected { reports } => stub_detected(reports),
        SteeringKind::RepeatedRead { content_hash } => repeated_read(content_hash),
        SteeringKind::TaskAlreadySatisfiedHint { test_command } => {
            task_already_satisfied_hint(test_command)
        }
        SteeringKind::ImplementNow {
            exploration_count,
            sample_paths,
        } => implement_now(*exploration_count, sample_paths),
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

fn stub_detected(reports: &[StubReportView]) -> String {
    let mut prompt = String::from(
        "STOP: Your implementation compiles but contains stub/placeholder code that must be \
         filled in. The following locations have incomplete implementations:\n\n",
    );
    for report in reports {
        let _ = write!(
            prompt,
            "- {}:{} -- {}\n  ```\n  {}\n  ```\n\n",
            report.path, report.line, report.pattern, report.context,
        );
    }
    prompt.push_str(
        "Replace ALL stubs with real, working implementations. Read the spec and codebase \
         to understand what each function should do, then implement it fully.\n\
         Do NOT use todo!(), unimplemented!(), Default::default() as a placeholder, or \
         ignore function parameters with _ prefixes.\n\
         After fixing, verify the build still passes, then call task_done.\n",
    );
    prompt
}

fn repeated_read(content_hash: &str) -> String {
    let short: String = content_hash
        .chars()
        .take(aura_config::REPEATED_READ_HASH_DISPLAY_CHARS)
        .collect();
    format!(
        "You've already read these exact bytes (content_hash={short}) 3 times this turn. \
         Use `start_line`/`end_line` to narrow the request, or move on — the file hasn't \
         changed."
    )
}

fn implement_now(exploration_count: usize, sample_paths: &[String]) -> String {
    let paths_line = if sample_paths.is_empty() {
        "none recorded yet".to_string()
    } else {
        sample_paths.join(", ")
    };
    format!(
        "You have read enough ({exploration_count} exploration tools, no file writes yet; \
         paths already inspected: {paths_line}). Your next tool calls must be `write_file` or \
         `edit_file` (or `delete_file` if appropriate). Do not call read_file, list_files, \
         find_files, stat_file, or search_code until you have created or changed at least one \
         file. If this task is already satisfied, call `task_done` with \
         `\"no_changes_needed\": true` and explain why in `\"notes\"`."
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
