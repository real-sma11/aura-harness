//! `SystemPromptBuilder` — the single producer of dev-loop and chat
//! system prompts (Phase 2a of the simplification plan).
//!
//! For PR B the builder concatenates the per-section renderers in
//! [`super::sections`] verbatim, leaving the four PR A golden
//! snapshots passing byte-for-byte. PR C will flip every section's
//! wrapper to the canonical `<tag>...</tag>` schema in a single
//! intentional snapshot regeneration.
//!
//! The builder is the *only* module that knows the canonical section
//! ordering; callers (`agentic_execution_system_prompt`,
//! `build_chat_system_prompt`) just compose the section list. That
//! way PR C's schema flip is a one-file change inside this directory.

use super::sections::{
    self, agent_identity as agent_identity_section, agent_skills as agent_skills_section,
    agent_system_prompt as agent_system_prompt_section, agents_md as agents_md_section,
    chat_capabilities as chat_capabilities_section, dev_loop_workflow as dev_loop_workflow_section,
    project_context as project_context_section, tool_discipline as tool_discipline_section,
};
use crate::prompts::{AgentIdentity, ProjectInfo};

/// Builder accumulating prompt section bytes in insertion order.
///
/// Each section method renders its body via the matching
/// [`super::sections`] submodule and pushes the resulting `String` (if
/// non-empty) onto an internal vector. [`SystemPromptBuilder::build`]
/// joins them with no separator — the per-section bodies bake their
/// own leading / trailing newlines so the join is byte-identical with
/// the legacy in-place `String::push_str` chains.
///
/// Empty inputs (None / empty list / blank string) translate to "skip
/// this section entirely", which is what every PR B caller exercises:
/// `agent_identity` / `agent_skills` / `agent_system_prompt` /
/// `tool_discipline` all push nothing today.
#[derive(Debug, Default)]
pub struct SystemPromptBuilder {
    sections: Vec<String>,
    last_agents_md_probe: Option<agents_md_section::AgentsMdProbe>,
}

impl SystemPromptBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append the agent-identity section, when populated.
    #[must_use]
    pub fn agent_identity(mut self, identity: Option<AgentIdentity<'_>>) -> Self {
        if let Some(text) = agent_identity_section::render(identity) {
            self.sections.push(text);
        }
        self
    }

    /// Append the agent-skills section, when the list is non-empty.
    #[must_use]
    pub fn agent_skills(mut self, skills: &[String]) -> Self {
        if let Some(text) = agent_skills_section::render(skills) {
            self.sections.push(text);
        }
        self
    }

    /// Append the operator-authored system prompt verbatim, when set.
    #[must_use]
    pub fn agent_system_prompt(mut self, prompt: Option<&str>) -> Self {
        if let Some(text) = agent_system_prompt_section::render(prompt) {
            self.sections.push(text);
        }
        self
    }

    /// Append the chat-style project-context block.
    ///
    /// Always emits content (Name / Description / Folder / Build /
    /// Test). The dev-loop builder doesn't call this in PR B because
    /// its build / test commands and platform info are still inlined
    /// into [`SystemPromptBuilder::dev_loop_workflow`] for byte-identical
    /// output; PR C consolidates the two paths.
    #[must_use]
    pub fn project_context(mut self, project: &ProjectInfo<'_>) -> Self {
        self.sections
            .push(project_context_section::render(project));
        self
    }

    /// Probe the workspace root for `AGENTS.md` (case-insensitive),
    /// append the file content as a dedicated section when found and
    /// within the byte cap, and remember the [`AgentsMdProbe`] outcome
    /// so callers can introspect the verdict via
    /// [`SystemPromptBuilder::agents_md_probe`].
    #[must_use]
    pub fn agents_md_from_workspace(mut self, folder_path: &str) -> Self {
        let mut buf = String::new();
        let probe = agents_md_section::append(&mut buf, folder_path);
        if !buf.is_empty() {
            self.sections.push(buf);
        }
        self.last_agents_md_probe = Some(probe);
        self
    }

    /// Append the dev-loop workflow + apply_patch + git-safety + code-quality
    /// block. `build_cmd` and `test_cmd` are spliced in verbatim.
    #[must_use]
    pub fn dev_loop_workflow(mut self, build_cmd: &str, test_cmd: &str) -> Self {
        self.sections
            .push(dev_loop_workflow_section::render(build_cmd, test_cmd));
        self
    }

    /// Append the tool-discipline section.
    ///
    /// PR B: no-op (the prose was deleted by the 2026-05 cook-loop
    /// strip). The method stays on the builder so the canonical
    /// section list mirrors the planned `<tag>` schema.
    #[must_use]
    pub fn tool_discipline(mut self) -> Self {
        if let Some(text) = tool_discipline_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the chat-capabilities prose
    /// ([`super::CHAT_SYSTEM_PROMPT_BASE`]) verbatim.
    #[must_use]
    pub fn chat_capabilities(mut self) -> Self {
        self.sections.push(chat_capabilities_section::render());
        self
    }

    /// Most recently observed AGENTS.md probe outcome, if any. Lets
    /// callers surface the verdict on operator dashboards / event
    /// streams without re-probing the filesystem.
    #[must_use]
    pub fn agents_md_probe(&self) -> Option<&agents_md_section::AgentsMdProbe> {
        self.last_agents_md_probe.as_ref()
    }

    /// Concatenate every accumulated section in insertion order. The
    /// section bodies own their leading / trailing whitespace, so the
    /// join is just a `String::join("")`.
    #[must_use]
    pub fn build(self) -> String {
        self.sections.join("")
    }
}

// Convenience re-export so external crates that want the probe enum
// (e.g. for surfacing on the run event stream in PR C) don't need to
// reach into `super::sections` directly.
pub use sections::{probe_agents_md, AgentsMdProbe};
