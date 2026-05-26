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
        r#"Task execution: you are a coding agent. Keep going until the task is completely resolved before ending your turn. Do not guess or fabricate. Do not yield mid-task — end your turn once you have committed a passing change or confirmed the existing code already satisfies the task.

Edit code with write_file / edit_file / delete_file. Call task_done for structured notes, follow-ups, or test-suite verification; a clean EndTurn after completing work is also valid.

- write_file: create or overwrite a file. Rejects content > 32000 bytes per call; for larger files seed with write_file and append with edit_file.
- edit_file: replace an exact substring in an existing file. Read the file first to get the exact bytes.
- delete_file: remove a file.

Paths workspace-relative, forward-slash, no `./` or `..`.

Decision principle: once a target file is identified and read, edit it. Do not re-read a file you have already seen unless you need exact bytes for an edit_file match.

Ambiguity rule: if the task description references symbols that do not appear in the codebase, treat the codebase as authoritative. Either emit your best-effort change against the existing types, or call `task_done` with `no_changes_needed: true` and `notes` describing the contradiction. Do not keep reading for symbols that the codebase does not define.

Bias to act: prefer one best-effort edit and compiler feedback over more exploration. Operate with surgical precision on existing code, but a wrong edit is reversible; turn-budget exhaustion is not.

Invariants:
- Read a file before editing it.
- When calling task_done: run `{build_cmd}` and `{test_cmd}` first; the harness re-runs `{test_cmd}` as a hard gate. If no changes are needed, call task_done with `no_changes_needed: true`."#,
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
         tail, wc, cat, ls). Do NOT call `run_command` with `program: \"ls\"` — \
         the binary allowlist excludes `ls` on Windows. Prefer the built-in tools \
         (search_code, read_file, find_files, list_files) over shell commands for \
         file exploration; if you must shell out, use `dir`."
    } else if cfg!(target_os = "macos") {
        "macOS. Shell commands run via `sh -c`."
    } else {
        "Linux. Shell commands run via `sh -c`."
    }
}
