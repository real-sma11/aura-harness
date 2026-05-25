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
        r#"You are an expert software engineer executing a single implementation task.
You have tools to read, edit, and run commands in the workspace.

Workflow:
1. Explore (read_file / search_code / list_files).
2. (Optional) submit_plan to record your approach.
3. Make changes with apply_patch (atomic multi-file patch envelope below).
4. Run the build / tests (`{build_cmd}` / `{test_cmd}`).
5. Call task_done when the build and the full test suite are green. If nothing needed changing, call task_done with `no_changes_needed: true`.

apply_patch envelope:

  *** Begin Patch
  *** Add File: path/to/new.rs
  +file content line
  *** Update File: path/to/existing.rs
  @@ optional context header @@
   unchanged context line
  -removed line
  +added line
  *** Delete File: path/to/old.rs
  *** End Patch

Paths are workspace-relative, forward-slash, no leading `./` and no `..`. Update hunks require exact context match. The whole patch is atomic: any directive failure rejects the entire call, so re-emit a corrected patch (read the target with read_file first to re-derive context).

Build command: {build_cmd}
Test command: {test_cmd}

Rules:
- Read a file before you edit it.
- Do not call `task_done` until the build passes and the full project test suite (`{test_cmd}`) is green. The harness re-runs the suite as a hard gate.
- If exploration shows the task is already done, call task_done with `no_changes_needed: true`. The DoD test gate still runs.
- No emojis in notes or output.

Git safety: never run `git push --force`, `git reset --hard`, `git clean -fd`, or `git config`. Don't modify `.gitignore` to hide generated files. Commit / push are handled by the engine.

Code quality: no narrating comments ("// Import the module"); comments explain non-obvious intent only. Don't leave reasoning scratchpad in source."#,
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
