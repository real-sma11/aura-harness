use super::ProjectInfo;

pub mod sections;

pub const CHAT_SYSTEM_PROMPT_BASE: &str = r#"You are Aura, an AI software engineering assistant embedded in a project management and code execution platform.

You have access to tools that let you directly manage the user's project:
- **Specs**: list, create, update, delete technical specifications
- **Tasks**: list, create, update, delete, transition status, trigger execution
- **Project**: view and update project settings (name, description, build/test commands)
- **Dev Loop**: start, pause, or stop the autonomous development loop
- **Filesystem**: read, write, edit, delete files and list directories in the project folder
  - Only read paths that exist. When generating or refining a spec for a product/project whose layout is being described (e.g. "Spectron has crates spectron-core, spectron-storage..."), those paths are the *target* layout, not the current repo. Use list_files to see what is actually in the project folder; do not assume paths from the spec exist on disk.
- **Search**: search_code for regex pattern search, find_files for glob matching
- **Shell**: run_command to execute build, test, git, or other commands
- **Progress**: view task completion metrics

When the user asks you to create, modify, or manage project artifacts, USE YOUR TOOLS to do it directly rather than just describing what to do. Be proactive -- if the user says "add a task for X", call create_task. If they say "show me the specs", call list_specs.

Spec creation and task creation are always two distinct steps. Never create tasks in the same turn as creating specs. Step 1: create or finalize all specs. Step 2: only after specs exist, create or extract tasks (e.g. via "Extract tasks" or create_task in a follow-up).

CRITICAL -- Planning vs Execution boundary:
After creating tasks (via create_task or task extraction), STOP. Summarize what was created and wait for the user. Do NOT proceed to implement tasks by calling write_file, edit_file, run_task, or start_dev_loop unless the user explicitly asks you to implement, start the dev loop, or run a task. Task implementation is the job of the autonomous dev loop, which the user starts via the UI or by asking you to start it. Your role after task creation is to report the result and wait for further instructions.
Filesystem tools (write_file, edit_file) may still be used for direct user requests unrelated to task execution (e.g. "create a .gitignore", "update the README").

When the user provides a requirements document or spec (pasted text or asks to "turn this into specs"):
- Split it into multiple logical specs ordered from most foundational to least (e.g. 01: Core Types, 02: Persistence, 03: API). Call create_spec once per section. Check list_specs first to number sequentially and avoid duplicates. Do NOT call create_task in this turn; task creation is a separate step after all specs are created.
- For each spec use the same structure as the project spec generator: title format two-digit number + colon + space + name (e.g. "01: Core Domain Types"); markdown must include Purpose, Major concepts, Interfaces (code-level), a Tasks section as a table with columns ID, Task, Description (task IDs as <spec_number>.<task_number>, e.g. 1.0, 1.1, 1.2), Key behaviors, and Test criteria. Add mermaid diagrams where useful.

When creating specs with create_spec (single spec):
- Title format: two-digit zero-padded number + colon + space + short name (e.g. "01: Core Domain Types")
- Number specs sequentially based on existing specs (check with list_specs first)
- Do NOT use em dashes (---) in the title

When using get_spec, update_spec, delete_spec, or task tools that require a spec_id or task_id, always use the UUID returned by list_specs, list_tasks, or create_spec/create_task. Never use the title number (e.g. "01") as the ID.

For conversational questions about architecture, debugging, or best practices, respond with helpful text.

Use markdown formatting for code blocks and structured responses. Be concise. Do NOT use emojis in your responses."#;

// ---------------------------------------------------------------------------
// Agentic execution system prompt
// ---------------------------------------------------------------------------

#[must_use]
pub fn agentic_execution_system_prompt(project: &ProjectInfo<'_>) -> String {
    let build_cmd = project.build_command.unwrap_or("(not configured)");
    // Prefer the operator's env override so the prompt shows the agent the
    // exact command the DoD gate will actually run. This keeps the agent's
    // mental model in sync with the gate when an operator has redirected
    // it (e.g. `AURA_DOD_TEST_COMMAND="pytest -q -k smoke"`). When no
    // override is set, fall back to the project config or the placeholder.
    let resolved_test_cmd = std::env::var(crate::task_executor::TEST_COMMAND_OVERRIDE_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let test_cmd = resolved_test_cmd
        .as_deref()
        .or(project.test_command)
        .unwrap_or("(not configured)");

    let platform_info = platform_info_string();

    let mut prompt = format!(
        r#"You are an expert software engineer executing a single implementation task.
You have tools to read, edit, and run commands in the workspace.

{platform_info}

Workflow:
1. Explore (read_file / search_code / list_files).
2. (Optional) call submit_plan to record your approach. Not required.
3. Make the changes with apply_patch (see WRITES — APPLY_PATCH below). One call can add, update, and delete multiple files atomically.
4. Run the build / tests as needed (`{build_cmd}` / `{test_cmd}`).
5. Call task_done when the changes compile and the test suite is green. If no changes were required, call task_done with `no_changes_needed: true`.

WRITES — APPLY_PATCH:
The dev-loop has ONE write primitive: apply_patch. It takes a single `patch` string argument containing a multi-file patch in this envelope format:

  *** Begin Patch
  *** Add File: path/to/new.rs
  +file content line 1
  +file content line 2
  *** Update File: path/to/existing.rs
  @@ optional context header @@
   unchanged context line
  -removed line
  +added line
  *** Delete File: path/to/old.rs
  *** End Patch

Rules:
- Paths are workspace-relative, forward-slash, no leading `./` and no `..`.
- Add File body: every following line starting with `+` is the literal file content (the `+` is stripped). Indentation is preserved.
- Update File body: one or more hunks. Each hunk starts with `@@ ... @@`. Inside a hunk, ` `-prefixed lines must match the file exactly (context); `-`-prefixed lines are removed and must match; `+`-prefixed lines are added.
- One apply_patch call can mix Add, Update, and Delete directives across multiple files. The call is atomic: every directive is validated against the on-disk state first; if any directive fails (parse error, context mismatch, missing target, target already exists for Add), NONE of the changes are applied. Re-emit a corrected patch on the next turn.
- The error returned will name the offending file/hunk so you can fix the context lines. Read the target file with `read_file` to re-derive context from real bytes before retrying.

Build command: {build_cmd}
Test command: {test_cmd}

Rules:
- Read a file before you edit it.
- For Rust source: ASCII only, raw string literals for multi-line strings, `serde_json::json!()` for JSON in tests.
- For TypeScript: forward slashes in import paths.
- Do not call `task_done` until the build passes and the full project test suite (`{test_cmd}`) is green. The harness re-runs the suite as a hard gate.
- If exploration reveals the task is already done (e.g. a prior task implemented it, or the change is a no-op), call task_done with `no_changes_needed: true` and explain in `notes`. The DoD test gate still runs, but file-op enforcement is bypassed.
- Do not output raw JSON with `file_ops` in text responses; use the tools.
- No emojis in notes or output.

GIT SAFETY:
- Never run `git push --force`, `git reset --hard`, or `git clean -fd`.
- Never modify `.gitignore` to hide generated files.
- Never run `git config` to change user identity.
- Commit / push are handled by the engine; don't invoke them yourself unless the task explicitly requires it.

CODE QUALITY:
- No narrating comments ("// Import the module", "// Return the result"). Comments explain non-obvious intent only.
- Don't leave reasoning scratchpad in source.
"#,
    );

    append_agents_md(&mut prompt, project.folder_path);

    prompt
}

const fn platform_info_string() -> &'static str {
    if cfg!(windows) {
        "Platform: Windows. Shell commands run via `cmd /C`. Use PowerShell or \
         Windows-compatible syntax. Avoid Unix-only tools (grep, sed, awk, head, \
         tail, wc, cat). Prefer the built-in tools (search_code, read_file, \
         find_files, list_files) over shell commands for file exploration."
    } else if cfg!(target_os = "macos") {
        "Platform: macOS. Shell commands run via `sh -c`."
    } else {
        "Platform: Linux. Shell commands run via `sh -c`."
    }
}

// ---------------------------------------------------------------------------
// Chat system prompt builder
// ---------------------------------------------------------------------------

#[must_use]
pub fn build_chat_system_prompt(project: &ProjectInfo<'_>, custom_system_prompt: &str) -> String {
    let mut prompt = if custom_system_prompt.is_empty() {
        CHAT_SYSTEM_PROMPT_BASE.to_string()
    } else {
        let mut p = custom_system_prompt.to_string();
        p.push_str("\n\n");
        p.push_str(CHAT_SYSTEM_PROMPT_BASE);
        p
    };

    prompt.push_str(&format!(
        "\n\n## Current Project\n- **Name**: {}\n- **Description**: {}\n- **Folder**: {}\n- **Build**: {}\n- **Test**: {}\n",
        project.name,
        project.description,
        project.folder_path,
        project.build_command.unwrap_or("(not set)"),
        project.test_command.unwrap_or("(not set)"),
    ));

    append_agents_md(&mut prompt, project.folder_path);
    append_tech_stack(&mut prompt, project.folder_path);
    prompt
}

fn append_tech_stack(prompt: &mut String, folder_path: &str) {
    let folder = std::path::Path::new(folder_path);
    if !folder.is_dir() {
        return;
    }

    let mut stack: Vec<&str> = Vec::new();
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "Rust"),
        ("package.json", "Node.js/TypeScript"),
        ("pyproject.toml", "Python"),
        ("requirements.txt", "Python"),
        ("go.mod", "Go"),
        ("pom.xml", "Java/Maven"),
        ("build.gradle", "Java/Gradle"),
        ("Gemfile", "Ruby"),
        ("composer.json", "PHP"),
        ("mix.exs", "Elixir"),
    ];
    for (file, tech) in markers {
        if folder.join(file).exists() && !stack.contains(tech) {
            stack.push(tech);
        }
    }
    if !stack.is_empty() {
        prompt.push_str(&format!("- **Tech Stack**: {}\n", stack.join(", ")));
    }

    append_directory_listing(prompt, folder);
    append_config_previews(prompt, folder);
}

fn append_directory_listing(prompt: &mut String, folder: &std::path::Path) {
    let entries = match std::fs::read_dir(folder) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut items: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.')
            || name == "node_modules"
            || name == "target"
            || name == "__pycache__"
            || name == "dist"
            || name == "build"
        {
            continue;
        }
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        items.push(if is_dir { format!("{name}/") } else { name });
    }
    items.sort();
    if !items.is_empty() {
        let listing = items
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_str(&format!("\n### Project Structure\n{listing}\n"));
    }
}

fn append_config_previews(prompt: &mut String, folder: &std::path::Path) {
    let config_files: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "tsconfig.json",
        "pyproject.toml",
    ];
    let mut config_budget: usize = 2000;
    let mut config_sections: Vec<String> = Vec::new();
    for &cf in config_files {
        if config_budget == 0 {
            break;
        }
        let path = folder.join(cf);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let preview: String = content.lines().take(30).collect::<Vec<_>>().join("\n");
            let preview = if preview.len() > config_budget {
                preview[..config_budget].to_string()
            } else {
                preview
            };
            config_budget = config_budget.saturating_sub(preview.len());
            config_sections.push(format!("**{cf}**:\n```\n{preview}\n```"));
        }
    }
    if !config_sections.is_empty() {
        prompt.push_str("\n### Key Config Files\n");
        prompt.push_str(&config_sections.join("\n"));
        prompt.push('\n');
    }
}

/// Hard cap on AGENTS.md bytes injected into the system prompt. Larger
/// files are skipped (with a warn log) rather than truncated so the
/// agent never reads a half-instruction.
pub(crate) const AGENTS_MD_MAX_BYTES: usize = 64 * 1024;

/// Header used when an AGENTS.md is found at the workspace root. Kept
/// as a `const` so callers and tests can both reference the canonical
/// wording instead of duplicating the literal.
pub(crate) const AGENTS_MD_SECTION_HEADER: &str = "## Project AGENTS.md";

/// Read the project root's `AGENTS.md` (case-insensitive) and append it
/// as a dedicated system-prompt section. No-op when the file is absent,
/// when `folder_path` is not a directory, or when the file exceeds the
/// byte cap.
///
/// We try a small set of explicit casing variants instead of doing a
/// full directory scan: the AGENTS.md convention is well-defined and
/// three `fs::read_to_string` probes are cheaper than enumerating the
/// workspace root.
fn append_agents_md(prompt: &mut String, folder_path: &str) {
    let folder = std::path::Path::new(folder_path);
    if !folder.is_dir() {
        return;
    }
    for variant in ["AGENTS.md", "agents.md", "Agents.md"] {
        let path = folder.join(variant);
        match std::fs::read_to_string(&path) {
            Ok(content) if content.len() <= AGENTS_MD_MAX_BYTES => {
                prompt.push_str(&format!(
                    "\n{header}\n\
                     The following instructions come from the project's `{variant}` file \
                     at the workspace root. Treat them as authoritative project-author \
                     guidance and follow them throughout this session.\n\n\
                     ```\n{content}\n```\n",
                    header = AGENTS_MD_SECTION_HEADER,
                ));
                return;
            }
            Ok(content) => {
                tracing::warn!(
                    bytes = content.len(),
                    cap = AGENTS_MD_MAX_BYTES,
                    variant,
                    "AGENTS.md exceeded byte cap; skipping injection",
                );
                return;
            }
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests;
