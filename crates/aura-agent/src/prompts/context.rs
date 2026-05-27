use super::{ProjectInfo, SessionInfo, SpecInfo, TaskInfo};

/// Default max bytes the spec.markdown_contents section may contribute to
/// the dev-loop bootstrap user message. The full spec is duplicative of
/// the task description for the case where one spec has one task, and
/// it inflates the request body with the same code patterns twice. The
/// upstream Cloudflare WAF in front of `aura-router.onrender.com` blocks
/// dev-loop bootstrap bodies whose user message contains a high density
/// of code-like content (Python slicing, `&` operators, file paths,
/// repeated escaped-amp `\u0026` sequences). Capping the spec section to
/// ~1.5KB plus a short "(truncated)" marker keeps the body well below
/// the empirically observed WAF cliff while preserving the spec title
/// and the high-level intent of the spec for context.
///
/// Tunable at runtime via `AURA_AGENT_BOOTSTRAP_SPEC_BYTES` to allow
/// experimentation without a rebuild. Set to `0` to skip the spec
/// markdown entirely (only the title is included).
const BOOTSTRAP_SPEC_DEFAULT_BYTES: usize = 1500;

fn bootstrap_spec_byte_budget() -> usize {
    std::env::var("AURA_AGENT_BOOTSTRAP_SPEC_BYTES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(BOOTSTRAP_SPEC_DEFAULT_BYTES)
}

/// Returns true if the bootstrap should strip fenced code blocks from
/// spec markdown and task descriptions. Default: disabled, so task agents
/// see full spec/task code in turn 0 instead of being told to `read_file`.
/// Set `AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES=1` (or `true`/`yes`/`on`) to
/// re-enable WAF-safe stripping when routing through `aura-router.onrender.com`'s
/// Cloudflare zone (e.g. Python slicing, `&` operators in fenced blocks).
fn bootstrap_should_strip_code_fences() -> bool {
    matches!(
        std::env::var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// Strip fenced code blocks (```...```) from a markdown string, replacing
/// each one with a `[code example: <N> bytes elided to fit body cap]`
/// marker. Preserves all surrounding prose so the agent retains the
/// human-readable explanation. Indentation-based code blocks (4-space
/// indent) are left untouched on purpose: they are rare in our specs/
/// task descriptions and harder to detect reliably.
fn strip_fenced_code_blocks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_fence = false;
    let mut fence_buf = String::new();
    for line in input.split_inclusive('\n') {
        // A fence line starts with three backticks, optionally followed
        // by a language tag and trailing whitespace. We accept both
        // ```python and ```python\n.
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let is_fence = trimmed.trim_start().starts_with("```");
        if !in_fence && is_fence {
            in_fence = true;
            fence_buf.clear();
            continue;
        }
        if in_fence && is_fence {
            in_fence = false;
            let elided_bytes = fence_buf.len();
            out.push_str(&format!(
                "[code example: {elided_bytes} bytes elided to fit body cap; \
                 read the source files directly with `read_file` if needed]\n"
            ));
            fence_buf.clear();
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
        } else {
            out.push_str(line);
        }
    }
    if in_fence {
        // Unterminated fence: keep the buffered content verbatim so we
        // never silently drop content; the WAF risk in this rare case
        // is preferable to silently losing instructions.
        out.push_str("```\n");
        out.push_str(&fence_buf);
    }
    out
}

fn append_spec_section(ctx: &mut String, spec: &SpecInfo<'_>, budget: usize) {
    if budget == 0 || spec.markdown_contents.is_empty() {
        ctx.push_str(&format!("# Spec: {}\n\n", spec.title));
        return;
    }
    let sanitized;
    let body: &str = if bootstrap_should_strip_code_fences() {
        sanitized = strip_fenced_code_blocks(spec.markdown_contents);
        &sanitized
    } else {
        spec.markdown_contents
    };
    if body.len() <= budget {
        ctx.push_str(&format!("# Spec: {}\n{body}\n\n", spec.title));
        return;
    }
    // Truncate at a UTF-8 boundary so the resulting string is valid Rust
    // text. Walk char_indices forward and stop at the last index <=
    // budget; this guarantees we never split a multi-byte char.
    let cut = body
        .char_indices()
        .take_while(|(i, _)| *i < budget)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let kept = &body[..cut];
    let omitted = body.len() - cut;
    ctx.push_str(&format!(
        "# Spec: {}\n{kept}\n\n[... spec truncated to fit body cap; omitted {omitted} bytes. \
         The task description below carries the actionable content for this turn.]\n\n",
        spec.title
    ));
}

fn task_description_for_bootstrap(task: &TaskInfo<'_>) -> String {
    if bootstrap_should_strip_code_fences() {
        strip_fenced_code_blocks(task.description)
    } else {
        task.description.to_string()
    }
}

/// Build the initial user-message context for an agentic task run.
///
/// `attempt` is the 0-indexed retry counter — 0 for the first run, 1+
/// for retries. `enrichment_block` is the pre-resolved
/// paths/symbols/file-heads markdown produced by the
/// [`crate::prompts::enrichment`] module. It is only spliced when
/// `attempt == 0` and the block is non-empty: on retries, Phase 5's
/// decomposition will provide a different, narrower context and the
/// resolve cost is wasted on re-runs of the same task.
#[must_use]
pub fn build_agentic_task_context(
    project: &ProjectInfo<'_>,
    spec: &SpecInfo<'_>,
    task: &TaskInfo<'_>,
    session: &SessionInfo<'_>,
    completed_deps: &[TaskInfo<'_>],
    work_log_summary: &str,
    attempt: u32,
    enrichment_block: Option<&str>,
) -> String {
    let mut ctx = String::new();
    ctx.push_str(&format!(
        "# Project: {}\n{}\n\n",
        project.name, project.description
    ));
    append_spec_section(&mut ctx, spec, bootstrap_spec_byte_budget());
    let task_desc = task_description_for_bootstrap(task);
    ctx.push_str(&format!("# Task: {}\n{task_desc}\n\n", task.title));

    // Phase 4: splice the pre-resolved paths/symbols block immediately
    // after the task description, but only on the first attempt. On
    // retries, the agent already saw this once and Phase 5's
    // decomposition (when wired) provides a different starting context;
    // re-injecting the same hints just wastes tokens and hides the
    // signal that this is a re-run.
    if attempt == 0 {
        if let Some(block) = enrichment_block {
            let trimmed = block.trim();
            if !trimmed.is_empty() {
                ctx.push_str(trimmed);
                ctx.push_str("\n\n");
            }
        }
    }

    if !session.summary_of_previous_context.is_empty() {
        ctx.push_str(&format!(
            "# Previous Context Summary\n{}\n\n",
            session.summary_of_previous_context
        ));
    }
    if !task.execution_notes.is_empty() {
        ctx.push_str(&format!(
            "# Notes from Prior Attempts\n{}\n\n",
            task.execution_notes
        ));
    }

    if !completed_deps.is_empty() {
        ctx.push_str("# Completed Predecessor Tasks\n");
        ctx.push_str(&format_completed_deps(completed_deps));
        ctx.push('\n');
    }

    if !work_log_summary.is_empty() {
        ctx.push_str(&format!(
            "# Session Progress (tasks completed so far)\n{work_log_summary}\n\n\
             If this task's work was already done by a prior task, call task_done with \
             `no_changes_needed: true` instead of re-implementing.\n\n"
        ));
    }

    // Stripped (2026-05): previously this pushed a "Briefly explore...
    // form a plan... begin implementing" preamble plus, when the task
    // mentioned "test" / "integration", an extra "verify exact field
    // names, constructor signatures... before writing any test code"
    // block. Together they pushed the agent into long read-only turns
    // (49 tool calls / 0 file ops in the round-1 validation run). Hold
    // the user message at the task header plus a one-line directive.
    ctx.push_str("Make the changes this task requires, then call task_done.\n");

    ctx
}

fn format_completed_deps(completed_deps: &[TaskInfo<'_>]) -> String {
    let mut output = String::new();
    let mut dep_budget = 5_000usize;
    for dep in completed_deps {
        let files_list = dep
            .files_changed
            .iter()
            .map(|fc| format!("{} ({})", fc.path, fc.op))
            .collect::<Vec<_>>()
            .join(", ");
        let section = format!(
            "## {}\n{}\nFiles: {}\n\n",
            dep.title, dep.execution_notes, files_list,
        );
        if section.len() > dep_budget {
            break;
        }
        dep_budget -= section.len();
        output.push_str(&section);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::FileChangeEntry;

    /// Serializes env-mutating bootstrap fence tests (process-global env).
    static BOOTSTRAP_FENCE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct BootstrapFenceEnvGuard {
        prior: Option<String>,
    }

    impl BootstrapFenceEnvGuard {
        fn capture() -> Self {
            Self {
                prior: std::env::var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES").ok(),
            }
        }
    }

    impl Drop for BootstrapFenceEnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES", v),
                None => std::env::remove_var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES"),
            }
        }
    }

    #[test]
    fn basic_context_contains_project_and_task() {
        let project = ProjectInfo {
            project_id: None,
            name: "myproj",
            description: "A test project",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "Spec 1",
            markdown_contents: "spec body",
        };
        let task = TaskInfo {
            title: "Do the thing",
            description: "Implement it",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(ctx.contains("myproj"));
        assert!(ctx.contains("Do the thing"));
        assert!(ctx.contains("Spec 1"));
    }

    #[test]
    fn context_includes_completed_deps() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let files = vec![FileChangeEntry {
            path: "src/lib.rs".into(),
            op: "modify".into(),
        }];
        let dep = TaskInfo {
            title: "Prior task",
            description: "Did stuff",
            execution_notes: "notes here",
            files_changed: &files,
        };
        let task = TaskInfo {
            title: "Current",
            description: "",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[dep], "", 0, None);
        assert!(ctx.contains("Prior task"));
        assert!(ctx.contains("src/lib.rs (modify)"));
    }

    #[test]
    fn strip_fenced_code_blocks_removes_python_block() {
        let input =
            "Here is prose.\n\n```python\nimport os\nos.system('rm -rf /')\n```\n\nMore prose.\n";
        let out = strip_fenced_code_blocks(input);
        assert!(out.contains("Here is prose."));
        assert!(out.contains("More prose."));
        assert!(!out.contains("import os"));
        assert!(!out.contains("os.system"));
        assert!(out.contains("[code example:"));
    }

    #[test]
    fn strip_fenced_code_blocks_handles_multiple_fences() {
        let input = "a\n```\none\n```\nb\n```rust\ntwo\n```\nc\n";
        let out = strip_fenced_code_blocks(input);
        assert!(out.contains("a\n"));
        assert!(out.contains("b\n"));
        assert!(out.contains("c\n"));
        assert!(!out.contains("one"));
        assert!(!out.contains("two"));
        assert_eq!(out.matches("[code example:").count(), 2);
    }

    #[test]
    fn strip_fenced_code_blocks_preserves_input_without_fences() {
        let input = "no fences here, just prose with `inline code` and `more`.\n";
        let out = strip_fenced_code_blocks(input);
        assert_eq!(out, input);
    }

    #[test]
    fn task_description_keeps_fences_for_bootstrap_by_default() {
        let _lock = BOOTSTRAP_FENCE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = BootstrapFenceEnvGuard::capture();
        std::env::remove_var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES");

        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "T",
            description: "Do this:\n\n```python\nm.Linear1D(10) & m.Linear1D(5)\n```\n\nDone.",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(ctx.contains("Do this:"));
        assert!(ctx.contains("Done."));
        assert!(ctx.contains("Linear1D"));
        assert!(!ctx.contains("[code example:"));
    }

    #[test]
    fn task_description_strips_fences_when_env_enabled() {
        let _lock = BOOTSTRAP_FENCE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = BootstrapFenceEnvGuard::capture();
        std::env::set_var("AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES", "1");

        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "T",
            description: "Do this:\n\n```python\nm.Linear1D(10) & m.Linear1D(5)\n```\n\nDone.",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(ctx.contains("Do this:"));
        assert!(ctx.contains("Done."));
        assert!(!ctx.contains("Linear1D"));
        assert!(ctx.contains("[code example:"));
    }

    #[test]
    fn long_spec_markdown_is_truncated_to_budget() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let big_md = "x".repeat(5_000);
        let spec = SpecInfo {
            title: "Big spec",
            markdown_contents: &big_md,
        };
        let task = TaskInfo {
            title: "Task",
            description: "Do it",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(ctx.contains("# Spec: Big spec"));
        assert!(ctx.contains("spec truncated to fit body cap"));
        assert!(
            ctx.len() < 5_000,
            "expected truncation to keep ctx small, got {}",
            ctx.len()
        );
    }

    #[test]
    fn short_spec_markdown_is_kept_intact() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "Tiny",
            markdown_contents: "tiny body",
        };
        let task = TaskInfo {
            title: "T",
            description: "d",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(ctx.contains("tiny body"));
        assert!(!ctx.contains("spec truncated"));
    }

    /// Round-2 strip: the test/integration-task verify-before-writing
    /// injection was removed (see `build_agentic_task_context` doc
    /// comment). Pin the absence so the block doesn't sneak back in.
    #[test]
    fn context_omits_test_warning_for_test_tasks() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "Add integration tests",
            description: "Write tests for the API",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx = build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, None);
        assert!(
            !ctx.contains("This task involves writing tests"),
            "verify-before-writing block must stay out of the task context"
        );
        assert!(
            !ctx.contains("Briefly explore"),
            "explore-first preamble must stay out of the task context"
        );
        assert!(
            ctx.contains("Make the changes this task requires, then call task_done."),
            "round-2 directive must replace the deleted preamble"
        );
    }

    /// Phase 4: on `attempt == 0` an enrichment block is spliced in
    /// immediately after the task header.
    #[test]
    fn build_agentic_task_context_first_attempt_includes_block() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "Implement enqueue",
            description: "Wire Publisher::enqueue in crates/zero-network/src/publisher.rs",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let block = "## Pre-resolved context (from task description)\n\n\
                     Files mentioned in the task that exist in the workspace:\n\
                     - `crates/zero-network/src/publisher.rs`\n\n\
                     Use these as starting points; you do NOT need to re-list \
                     the directory or re-grep for these symbols.\n";
        let ctx =
            build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, Some(block));
        assert!(
            ctx.contains("## Pre-resolved context (from task description)"),
            "first attempt must include the enrichment header"
        );
        assert!(
            ctx.contains("crates/zero-network/src/publisher.rs"),
            "first attempt must include the resolved path"
        );
        // The block must land between the task header and the trailing
        // directive so the model reads it before the "make changes"
        // instruction. Empirically: task header index < block index <
        // directive index.
        let task_idx = ctx
            .find("# Task: Implement enqueue")
            .expect("task header present");
        let block_idx = ctx.find("## Pre-resolved context").expect("block present");
        let directive_idx = ctx
            .find("Make the changes this task requires")
            .expect("directive present");
        assert!(
            task_idx < block_idx && block_idx < directive_idx,
            "block must be spliced between task header and directive \
             (task={task_idx}, block={block_idx}, directive={directive_idx})"
        );
    }

    /// Phase 4: on `attempt > 0` the enrichment block is suppressed
    /// even when the caller passes one, so retries don't pay the
    /// resolve cost twice.
    #[test]
    fn build_agentic_task_context_retry_attempt_skips_block() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "Implement enqueue",
            description: "Wire Publisher::enqueue in crates/zero-network/src/publisher.rs",
            execution_notes: "previous attempt timed out",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let block = "## Pre-resolved context (from task description)\n\n\
                     Files mentioned in the task that exist in the workspace:\n\
                     - `crates/zero-network/src/publisher.rs`\n";
        let ctx =
            build_agentic_task_context(&project, &spec, &task, &session, &[], "", 1, Some(block));
        assert!(
            !ctx.contains("## Pre-resolved context"),
            "retry must NOT include the enrichment header, got:\n{ctx}"
        );
    }

    /// Phase 4: an empty block on attempt 0 must not emit an orphan
    /// header. Tests the trim-and-skip path in `build_agentic_task_context`.
    #[test]
    fn build_agentic_task_context_empty_block_is_skipped() {
        let project = ProjectInfo {
            project_id: None,
            name: "p",
            description: "",
            folder_path: "/tmp",
            build_command: None,
            test_command: None,
        };
        let spec = SpecInfo {
            title: "s",
            markdown_contents: "",
        };
        let task = TaskInfo {
            title: "T",
            description: "d",
            execution_notes: "",
            files_changed: &[],
        };
        let session = SessionInfo {
            summary_of_previous_context: "",
        };
        let ctx =
            build_agentic_task_context(&project, &spec, &task, &session, &[], "", 0, Some("   \n"));
        assert!(
            !ctx.contains("Pre-resolved context"),
            "empty/whitespace block must not produce a header"
        );
    }
}
