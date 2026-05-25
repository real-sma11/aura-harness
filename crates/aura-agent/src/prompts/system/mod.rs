//! System-prompt assembly for the dev-loop and chat paths.
//!
//! PR B reshapes this module from a pair of `format!` blobs into a
//! thin façade over [`SystemPromptBuilder`] + per-section modules in
//! [`sections`]. The bytes produced by [`agentic_execution_system_prompt`]
//! and [`build_chat_system_prompt`] are unchanged — the four PR A
//! golden snapshots in `__snapshots__/` are the contract. PR C will
//! flip every section's wrapper to the canonical `<tag>...</tag>`
//! schema with one intentional snapshot regeneration.
//!
//! What still lives at this top level:
//!
//! - [`CHAT_SYSTEM_PROMPT_BASE`]: the chat capabilities prose. Kept
//!   here so external `crate::prompts::CHAT_SYSTEM_PROMPT_BASE`
//!   re-exports continue to resolve. PR C will hoist ownership into
//!   [`sections::chat_capabilities`].
//! - The chat-side `append_tech_stack` / `append_directory_listing` /
//!   `append_config_previews` helpers — the plan defers their move
//!   into a section module to PR C alongside the chat refactor.

use super::{AgentInfo, ProjectInfo};

mod builder;
pub mod sections;

pub use builder::{probe_agents_md, AgentsMdProbe, SystemPromptBuilder};

#[cfg(test)]
pub(crate) use sections::dev_loop_workflow::platform_info_string;
#[cfg(test)]
pub(crate) use sections::{AGENTS_MD_MAX_BYTES, AGENTS_MD_SECTION_HEADER};

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

/// Build the dev-loop system prompt.
///
/// PR B re-adds the optional `agent` parameter so identity / skills /
/// operator-authored prompt can flow through into the assembled output
/// once aura-os populates the wire fields in PR C. Callers that have
/// no agent context (the dev-loop / task-run automatons today) pass
/// `None` and the rendered bytes match the PR A snapshots verbatim.
#[must_use]
pub fn agentic_execution_system_prompt(
    project: &ProjectInfo<'_>,
    agent: Option<&AgentInfo<'_>>,
) -> String {
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

    let identity = agent.and_then(|a| a.identity);
    let skills_owned: Vec<String> = agent.map(|a| a.skills.to_vec()).unwrap_or_default();
    let agent_system_prompt = agent.and_then(|a| a.system_prompt);

    SystemPromptBuilder::new()
        .agent_identity(identity)
        .agent_skills(&skills_owned)
        .agent_system_prompt(agent_system_prompt)
        .dev_loop_workflow(build_cmd, test_cmd)
        .tool_discipline()
        .agents_md_from_workspace(project.folder_path)
        .build()
}

// ---------------------------------------------------------------------------
// Chat system prompt builder
// ---------------------------------------------------------------------------

#[must_use]
pub fn build_chat_system_prompt(project: &ProjectInfo<'_>, custom_system_prompt: &str) -> String {
    let mut prompt = String::new();
    if !custom_system_prompt.is_empty() {
        prompt.push_str(custom_system_prompt);
        prompt.push_str("\n\n");
    }
    prompt.push_str(
        &SystemPromptBuilder::new()
            .chat_capabilities()
            .project_context(project)
            .agents_md_from_workspace(project.folder_path)
            .build(),
    );
    // PR B keeps the chat-only workspace-overview helpers (tech stack,
    // directory listing, config previews) at this top level. PR C will
    // either fold them into a `chat_workspace_overview` section module
    // or delete them outright; for now we just call them after the
    // builder so the byte layout is unchanged.
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

#[cfg(test)]
mod tests;
