//! `SystemPromptBuilder` — the single producer of dev-loop and chat
//! system prompts.
//!
//! Each section's [`super::sections`] module emits a self-contained
//! tag-wrapped body with no surrounding whitespace; the builder joins
//! the non-empty sections with a single blank line so the assembled
//! prompt reads as an ordered, labelled sequence.
//!
//! Empty sections (`None` from a renderer or an empty skills list)
//! are dropped — the builder never emits an empty tag.

use crate::descriptors::{AgentIdentity, AgentInfo, ProjectInfo};

use super::sections::{
    agent_identity as agent_identity_section, agent_skills as agent_skills_section,
    agent_system_prompt as agent_system_prompt_section, agents_md as agents_md_section,
    chat_capabilities as chat_capabilities_section, dev_loop_workflow as dev_loop_workflow_section,
    editing_etiquette as editing_etiquette_section, frontend_design as frontend_design_section,
    output_style as output_style_section, planning_guidance as planning_guidance_section,
    project_context as project_context_section, tool_discipline as tool_discipline_section,
};

pub use agents_md_section::{probe_agents_md, AgentsMdProbe};

/// Builder accumulating prompt section bytes in insertion order.
///
/// Each section method renders its body via the matching
/// [`super::sections`] submodule and pushes the resulting tag-wrapped
/// `String` (if non-empty) onto an internal vector.
/// [`SystemPromptBuilder::build`] joins the sections with a single
/// blank line (`"\n\n"`) so the canonical ordering shows up as a
/// readable sequence of labelled blocks.
///
/// Empty inputs (None / empty list / blank string) translate to "skip
/// this section entirely", which keeps the assembled prompt free of
/// `<agent_identity></agent_identity>` artefacts when the wire
/// payload is uninitialised.
#[derive(Debug, Default)]
pub struct SystemPromptBuilder {
    sections: Vec<String>,
    last_agents_md_probe: Option<AgentsMdProbe>,
}

impl SystemPromptBuilder {
    /// New empty builder.
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

    /// Append the `<project_context>` block (project metadata + host
    /// platform notice). Always emits content.
    #[must_use]
    pub fn project_context(mut self, project: &ProjectInfo<'_>) -> Self {
        self.sections.push(project_context_section::render(project));
        self
    }

    /// Probe the workspace root for `AGENTS.md` (case-insensitive),
    /// append the file content wrapped in
    /// `<agents_md path="...">...</agents_md>` when found and within
    /// the byte cap, and remember the [`AgentsMdProbe`] outcome so
    /// callers can introspect the verdict via
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

    /// Append the `<dev_loop_workflow>` block. `build_cmd` and
    /// `test_cmd` are spliced into the prose verbatim so the agent's
    /// mental model matches the DoD gate's invocation.
    #[must_use]
    pub fn dev_loop_workflow(mut self, build_cmd: &str, test_cmd: &str) -> Self {
        self.sections
            .push(dev_loop_workflow_section::render(build_cmd, test_cmd));
        self
    }

    /// Append the `<tool_discipline>` section.
    #[must_use]
    pub fn tool_discipline(mut self) -> Self {
        if let Some(text) = tool_discipline_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<editing_etiquette>` section.
    #[must_use]
    pub fn editing_etiquette(mut self) -> Self {
        if let Some(text) = editing_etiquette_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<planning_guidance>` section.
    #[must_use]
    pub fn planning_guidance(mut self) -> Self {
        if let Some(text) = planning_guidance_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<frontend_design>` section.
    #[must_use]
    pub fn frontend_design(mut self) -> Self {
        if let Some(text) = frontend_design_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<output_style>` section.
    #[must_use]
    pub fn output_style(mut self) -> Self {
        if let Some(text) = output_style_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<chat_capabilities>` section. Always non-empty.
    #[must_use]
    pub fn chat_capabilities(mut self) -> Self {
        self.sections.push(chat_capabilities_section::render());
        self
    }

    /// Most recently observed AGENTS.md probe outcome, if any. Lets
    /// callers surface the verdict on operator dashboards / event
    /// streams without re-probing the filesystem.
    #[must_use]
    pub fn agents_md_probe(&self) -> Option<&AgentsMdProbe> {
        self.last_agents_md_probe.as_ref()
    }

    /// Configure the dev-loop preset: `agent_identity →
    /// agent_skills → agent_system_prompt → project_context →
    /// agents_md → dev_loop_workflow → tool_discipline →
    /// editing_etiquette → planning_guidance → frontend_design →
    /// output_style`.
    #[must_use]
    pub fn preset_dev_loop(
        project: &ProjectInfo<'_>,
        agent: Option<&AgentInfo<'_>>,
        build_cmd: &str,
        test_cmd: &str,
    ) -> Self {
        let identity = agent.and_then(|a| a.identity);
        let skills_owned: Vec<String> = agent.map(|a| a.skills.to_vec()).unwrap_or_default();
        let agent_system_prompt = agent.and_then(|a| a.system_prompt);
        Self::new()
            .agent_identity(identity)
            .agent_skills(&skills_owned)
            .agent_system_prompt(agent_system_prompt)
            .project_context(project)
            .agents_md_from_workspace(project.folder_path)
            .dev_loop_workflow(build_cmd, test_cmd)
            .tool_discipline()
            .editing_etiquette()
            .planning_guidance()
            .frontend_design()
            .output_style()
    }

    /// Configure the chat preset: `chat_capabilities → agent_identity
    /// → agent_skills → agent_system_prompt → project_context →
    /// agents_md → editing_etiquette → frontend_design →
    /// output_style`. The chat path uses `chat_capabilities` instead
    /// of `dev_loop_workflow + tool_discipline + planning_guidance`.
    #[must_use]
    pub fn preset_chat(project: &ProjectInfo<'_>, agent: Option<&AgentInfo<'_>>) -> Self {
        let identity = agent.and_then(|a| a.identity);
        let skills_owned: Vec<String> = agent.map(|a| a.skills.to_vec()).unwrap_or_default();
        let agent_system_prompt = agent.and_then(|a| a.system_prompt);
        Self::new()
            .chat_capabilities()
            .agent_identity(identity)
            .agent_skills(&skills_owned)
            .agent_system_prompt(agent_system_prompt)
            .project_context(project)
            .agents_md_from_workspace(project.folder_path)
            .editing_etiquette()
            .frontend_design()
            .output_style()
    }

    /// Concatenate every accumulated section with a single blank line
    /// between non-empty entries (`"\n\n"`). Each section already
    /// owns its `<tag>...</tag>` envelope; the builder is only
    /// responsible for spacing.
    #[must_use]
    pub fn build(self) -> String {
        self.sections.join("\n\n")
    }
}
