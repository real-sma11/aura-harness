//! `SystemPromptBuilder` — the single producer of dev-loop and chat
//! system prompts.
//!
//! PR C flips every section to the canonical `<tag>...</tag>` schema.
//! Each section's [`super::sections`] module emits a self-contained
//! tag-wrapped body with no surrounding whitespace; the builder joins
//! the non-empty sections with a single blank line so the assembled
//! prompt reads as an ordered, labelled sequence:
//!
//! ```text
//! <agent_identity>...</agent_identity>
//!
//! <agent_skills>...</agent_skills>
//!
//! <project_context>...</project_context>
//!
//! <agents_md path="AGENTS.md">...</agents_md>
//!
//! <dev_loop_workflow>...</dev_loop_workflow>
//! ```
//!
//! Empty sections (`None` from a renderer or an empty skills list)
//! are dropped — the builder never emits an empty tag.

use super::sections::{
    self, agent_identity as agent_identity_section, agent_skills as agent_skills_section,
    agent_system_prompt as agent_system_prompt_section, agents_md as agents_md_section,
    chat_capabilities as chat_capabilities_section, dev_loop_workflow as dev_loop_workflow_section,
    editing_etiquette as editing_etiquette_section, frontend_design as frontend_design_section,
    output_style as output_style_section, planning_guidance as planning_guidance_section,
    project_context as project_context_section, tool_discipline as tool_discipline_section,
};
use crate::prompts::{AgentIdentity, ProjectInfo};

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

    /// Append the `<project_context>` block (project metadata + host
    /// platform notice). Always emits content; both the dev-loop and
    /// chat paths call this in PR C so the agent sees a consistent
    /// project descriptor regardless of which builder preset assembled
    /// the prompt.
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

    /// Append the `<dev_loop_workflow>` block (workflow + git safety +
    /// code quality). `build_cmd` and `test_cmd` are spliced into the
    /// prose verbatim so the agent's mental model matches the DoD
    /// gate's invocation.
    #[must_use]
    pub fn dev_loop_workflow(mut self, build_cmd: &str, test_cmd: &str) -> Self {
        self.sections
            .push(dev_loop_workflow_section::render(build_cmd, test_cmd));
        self
    }

    /// Append the `<tool_discipline>` section.
    ///
    /// PR C kept this method as a slot when the renderer returned
    /// `None`; the follow-up backfill refills the body with the
    /// narrow set of tool-call patterns the harness still enforces
    /// at runtime (the 32_000-byte `write_file` chunk guard and the
    /// `_redacted` / `<<<AURA_ELIDED_…>>>` placeholder rejection on
    /// `write_file` / `edit_file`). See
    /// [`super::sections::tool_discipline`] for the audit trail
    /// covering which rules were intentionally left out.
    #[must_use]
    pub fn tool_discipline(mut self) -> Self {
        if let Some(text) = tool_discipline_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<editing_etiquette>` section (codex-derived editing constraints).
    #[must_use]
    pub fn editing_etiquette(mut self) -> Self {
        if let Some(text) = editing_etiquette_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<planning_guidance>` section (dev-loop plan / submit_plan rules).
    #[must_use]
    pub fn planning_guidance(mut self) -> Self {
        if let Some(text) = planning_guidance_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<frontend_design>` section (codex-derived UI discipline).
    #[must_use]
    pub fn frontend_design(mut self) -> Self {
        if let Some(text) = frontend_design_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<output_style>` section (final-answer formatting rules).
    #[must_use]
    pub fn output_style(mut self) -> Self {
        if let Some(text) = output_style_section::render() {
            self.sections.push(text);
        }
        self
    }

    /// Append the `<chat_capabilities>` section
    /// ([`super::CHAT_SYSTEM_PROMPT_BASE`] wrapped in the canonical
    /// tag). Always non-empty.
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

    /// Concatenate every accumulated section with a single blank line
    /// between non-empty entries (`"\n\n"`). Each section already
    /// owns its `<tag>...</tag>` envelope; the builder is only
    /// responsible for spacing.
    #[must_use]
    pub fn build(self) -> String {
        self.sections.join("\n\n")
    }
}

// Convenience re-export so external crates that want the probe enum
// (e.g. for surfacing on the run event stream) don't need to reach
// into `super::sections` directly.
pub use sections::{probe_agents_md, AgentsMdProbe};
