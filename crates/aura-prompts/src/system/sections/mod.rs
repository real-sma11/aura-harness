//! Reusable prompt sections spliced into the assembled system
//! prompts.
//!
//! Each module owns the rendering for one
//! [`crate::system::SystemPromptBuilder`] method. Every module emits
//! a self-contained `<tag>...</tag>` body with no surrounding
//! whitespace; the builder joins the non-empty sections with a
//! single blank line so the assembled prompt reads as an ordered,
//! labelled sequence.

pub mod agent_identity;
pub mod agent_skills;
pub mod agent_system_prompt;
pub mod agents_md;
pub mod chat_capabilities;
pub mod dev_loop_workflow;
pub mod editing_etiquette;
pub mod frontend_design;
pub mod output_style;
pub mod planning_guidance;
pub mod project_context;
pub mod tool_discipline;

pub use agents_md::{
    probe_agents_md, AgentsMdProbe, AGENTS_MD_MAX_BYTES, AGENTS_MD_SECTION_TAG_PREFIX,
};
pub use chat_capabilities::CHAT_SYSTEM_PROMPT_BASE;
pub use dev_loop_workflow::platform_info_string;
