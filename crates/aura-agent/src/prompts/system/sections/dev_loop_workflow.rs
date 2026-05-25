//! `<dev_loop_workflow>`-bound section.
//!
//! PR C trimmed the historical workflow prose to a bare tools-and-invariants
//! listing on the theory that runtime gates would carry the execution
//! discipline. The post-mortem on Task 5.7 and 5.11 (see plan
//! `fix_dev-loop_doom_loop_021d485c.plan.md`) showed that without
//! shaping rules — a "keep going", a "do not guess", a decision /
//! ambiguity / bias-to-act / exit clause — the agent fills the
//! discipline void with thinking and never writes. Layer A refills
//! this section with codex-style execution discipline that does NOT
//! claim runtime enforcement (the harness still has no per-iteration
//! gates today); rules that mirror a live runtime gate stay in
//! [`super::tool_discipline`].
//!
//! Build / test commands remain inline so the agent's mental model
//! matches the DoD gate's invocation; platform info lives in
//! [`super::project_context`].

/// Render the dev-loop workflow block.
///
/// `build_cmd` and `test_cmd` are spliced into the prose verbatim; the
/// caller is responsible for substituting the env override / project
/// fallback / `(not configured)` placeholder before invoking us.
#[must_use]
pub(crate) fn render(build_cmd: &str, test_cmd: &str) -> String {
    let body = format!(
        r#"Task execution: you are a coding agent. Keep going until the task is completely resolved before ending your turn. Do not guess or fabricate. Do not yield mid-task — only stop once you have produced a passing change or called task_done with `no_changes_needed: true` and a `notes` explanation.

Edit code with write_file / edit_file / delete_file. Finish with task_done.

- write_file: create or overwrite a file. Rejects content > 32000 bytes per call; for larger files seed with write_file and append with edit_file.
- edit_file: replace an exact substring in an existing file. Read the file first to get the exact bytes.
- delete_file: remove a file.

Paths workspace-relative, forward-slash, no `./` or `..`.

Decision principle: once a target file is identified and read, edit it. Do not re-read a file you have already seen unless you need exact bytes for an edit_file match.

Ambiguity rule: if two files disagree on a name, type size, or field, treat the task description as authoritative. Pick one, edit it, then run the build — the compiler is more informative than another read.

Bias to act: prefer one best-effort edit and compiler feedback over more exploration. Operate with surgical precision on existing code, but a wrong edit is reversible; turn-budget exhaustion is not.

Exit clause: after three iterations without a write_file / edit_file / delete_file call, emit your best-effort change or call task_done with `no_changes_needed: true` and explain why in `notes`.

Invariants:
- Read a file before editing it.
- task_done only when `{build_cmd}` and `{test_cmd}` are both green; the harness re-runs `{test_cmd}` as a hard gate. If no changes are needed, call task_done with `no_changes_needed: true`.
- Never run: git push --force, git reset --hard, git clean -fd, git config. Do not touch .gitignore to hide build output.

When stuck on a decision (which way to resolve an ambiguity, which file to edit first), call submit_plan to publish your chosen approach before writing. It is transparent commitment surfaced to the operator, never a gate — writes are accepted with or without it."#,
    );
    format!("<dev_loop_workflow>\n{body}\n</dev_loop_workflow>")
}

/// Host-platform notice spliced into the project-context block in PR C
/// (`platform: ...` field). Pulled out as a `const fn` so test
/// scaffolding can scrub the line cross-platform and so
/// [`super::project_context`] can reuse it without re-implementing the
/// shell-dispatch matrix.
pub(crate) const fn platform_info_string() -> &'static str {
    if cfg!(windows) {
        "Windows. Shell commands run via `cmd /C`. Use PowerShell or \
         Windows-compatible syntax. Avoid Unix-only tools (grep, sed, awk, head, \
         tail, wc, cat). Prefer the built-in tools (search_code, read_file, \
         find_files, list_files) over shell commands for file exploration."
    } else if cfg!(target_os = "macos") {
        "macOS. Shell commands run via `sh -c`."
    } else {
        "Linux. Shell commands run via `sh -c`."
    }
}
