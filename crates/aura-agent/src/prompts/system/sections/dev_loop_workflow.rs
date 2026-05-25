//! `<dev_loop_workflow>`-bound section.
//!
//! PR C trims the historical workflow prose to the essentials (the
//! runtime gates are the hard guarantees) and wraps the result in
//! `<dev_loop_workflow>...</dev_loop_workflow>`. Build / test commands
//! remain inline so the agent's mental model matches the gate's
//! invocation; platform info has moved out to
//! [`super::project_context`].

/// Render the dev-loop workflow block.
///
/// `build_cmd` and `test_cmd` are spliced into the prose verbatim; the
/// caller is responsible for substituting the env override / project
/// fallback / `(not configured)` placeholder before invoking us.
#[must_use]
pub(crate) fn render(build_cmd: &str, test_cmd: &str) -> String {
    let body = format!(
        r#"Edit code with write_file / edit_file / delete_file. Finish with task_done.

- write_file: create or overwrite a file. Rejects content > 32000 bytes per call; for larger files seed with write_file and append with edit_file.
- edit_file: replace an exact substring in an existing file. Read the file first to get the exact bytes.
- delete_file: remove a file.

Paths workspace-relative, forward-slash, no `./` or `..`.

Invariants:
- Read a file before editing it.
- task_done only when `{build_cmd}` and `{test_cmd}` are both green; the harness re-runs `{test_cmd}` as a hard gate. If no changes are needed, call task_done with `no_changes_needed: true`.
- Never run: git push --force, git reset --hard, git clean -fd, git config. Do not touch .gitignore to hide build output.

Optional: call submit_plan after you have finished exploring and before you start editing — it records your plan for the operator and resets your exploration counters. Never required."#,
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
