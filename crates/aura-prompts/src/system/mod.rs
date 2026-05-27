//! System-prompt assembly for the dev-loop and chat paths.
//!
//! Both paths share the same section pool — they differ only in
//! which sections they include and in which order — so adding a new
//! section happens in exactly one place (`sections/` + a
//! `.builder.method()` call).
//!
//! Section ordering (insertion order = output order, blank-line
//! separated):
//!
//! - Dev loop: `agent_identity → agent_skills → agent_system_prompt
//!   → project_context → agents_md → dev_loop_workflow →
//!   tool_discipline → editing_etiquette → planning_guidance →
//!   frontend_design → output_style`.
//! - Chat: `chat_capabilities → agent_identity → agent_skills →
//!   agent_system_prompt → project_context → agents_md →
//!   editing_etiquette → frontend_design → output_style`. The chat
//!   path uses `chat_capabilities` *instead of*
//!   `dev_loop_workflow` + `tool_discipline` + `planning_guidance`.
//!
//! Empty sections (None / blank / empty list) are dropped, so the
//! assembled bytes never contain an empty tag.

mod builder;
pub mod sections;

#[cfg(test)]
mod tests;

pub use builder::{probe_agents_md, AgentsMdProbe, SystemPromptBuilder};
pub use sections::CHAT_SYSTEM_PROMPT_BASE;

use crate::descriptors::{AgentInfo, ProjectInfo};

/// Build the dev-loop system prompt.
///
/// Threads the optional `agent` parameter through to the identity /
/// skills / operator-prompt sections so a populated [`AgentInfo`]
/// produces the corresponding `<agent_identity>`, `<agent_skills>`,
/// and `<agent_system_prompt>` blocks. Callers without an agent
/// context pass `None` and those sections are dropped silently.
///
/// `test_command_override` is the operator's
/// `aura_config::agent().verify.test_command_override`; the caller
/// is responsible for resolving it from `aura-config` (this crate
/// is rendering-only). Passing `Some(cmd)` makes the rendered
/// prompt show the agent the exact command the DoD gate will run;
/// `None` falls back to `project.test_command`.
#[must_use]
pub fn agentic_execution_system_prompt(
    project: &ProjectInfo<'_>,
    agent: Option<&AgentInfo<'_>>,
    test_command_override: Option<&str>,
) -> String {
    let build_cmd = project.build_command.unwrap_or("(not configured)");
    let test_cmd = test_command_override
        .or(project.test_command)
        .unwrap_or("(not configured)");
    SystemPromptBuilder::preset_dev_loop(project, agent, build_cmd, test_cmd).build()
}

/// Build the chat-path system prompt.
///
/// A non-empty `custom_system_prompt` is prepended verbatim above
/// the builder output so operator overrides survive.
#[must_use]
pub fn build_chat_system_prompt(
    project: &ProjectInfo<'_>,
    custom_system_prompt: &str,
    agent: Option<&AgentInfo<'_>>,
) -> String {
    let mut prompt = String::new();
    if !custom_system_prompt.is_empty() {
        prompt.push_str(custom_system_prompt);
        prompt.push_str("\n\n");
    }
    prompt.push_str(&SystemPromptBuilder::preset_chat(project, agent).build());
    prompt
}

/// Default system prompt for the chat / TUI surfaces that did not
/// arrive with a baked-in prompt or any of the typed identity /
/// project_info wire fields.
#[must_use]
pub fn default_system_prompt() -> String {
    SystemPromptBuilder::new().chat_capabilities().build()
}
