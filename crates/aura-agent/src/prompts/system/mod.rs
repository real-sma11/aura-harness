//! System-prompt assembly for the dev-loop and chat paths.
//!
//! PR C flips the per-section renderers to the canonical
//! `<tag>...</tag>` schema and consolidates both paths behind
//! [`SystemPromptBuilder`]. The dev-loop and chat builders share the
//! same section pool — they differ only in which sections they
//! include and in which order — so adding a new section happens in
//! exactly one place (`sections/` + a `.builder.method()` call).
//!
//! Section ordering (insertion order = output order, blank-line
//! separated):
//!
//! - Dev loop: `agent_identity → agent_skills → agent_system_prompt
//!   → project_context → agents_md → dev_loop_workflow →
//!   tool_discipline`.
//! - Chat: `chat_capabilities → agent_identity → agent_skills →
//!   agent_system_prompt → project_context → agents_md`. The chat
//!   path uses `chat_capabilities` *instead of*
//!   `dev_loop_workflow` + `tool_discipline`.
//!
//! Empty sections (None / blank / empty list) are dropped, so the
//! assembled bytes never contain an empty tag.

use super::{AgentInfo, ProjectInfo};

mod builder;
pub mod sections;

pub use builder::{probe_agents_md, AgentsMdProbe, SystemPromptBuilder};

#[cfg(test)]
pub(crate) use sections::dev_loop_workflow::platform_info_string;
#[cfg(test)]
pub(crate) use sections::{AGENTS_MD_MAX_BYTES, AGENTS_MD_SECTION_TAG_PREFIX};

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
/// PR C threads the optional `agent` parameter through to the
/// identity / skills / operator-prompt sections so a populated
/// [`AgentInfo`] produces the corresponding `<agent_identity>`,
/// `<agent_skills>`, and `<agent_system_prompt>` blocks. Callers
/// without an agent context pass `None` and those sections are
/// dropped silently — the remaining sections (`project_context`,
/// `agents_md`, `dev_loop_workflow`, `tool_discipline`) always emit.
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
        .project_context(project)
        .agents_md_from_workspace(project.folder_path)
        .dev_loop_workflow(build_cmd, test_cmd)
        .tool_discipline()
        .build()
}

// ---------------------------------------------------------------------------
// Chat system prompt builder
// ---------------------------------------------------------------------------

/// Build the chat-path system prompt.
///
/// PR C drops the chat-only workspace-overview helpers
/// (`append_tech_stack` / `append_directory_listing` /
/// `append_config_previews`) per the simplification plan — the chat
/// path now emits the same labelled section set as the dev loop,
/// just with `<chat_capabilities>` in place of
/// `<dev_loop_workflow>` + `<tool_discipline>`. A non-empty
/// `custom_system_prompt` is still prepended verbatim above the
/// builder output so operator overrides survive the bracketed-schema
/// migration.
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
    prompt
}

#[cfg(test)]
mod tests;
