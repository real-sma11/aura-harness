//! Reusable prompt sections spliced into the assembled system prompts.
//!
//! Each module owns the rendering for one [`SystemPromptBuilder`]
//! method; in PR C every module flips its wrapper to the canonical
//! `<tag>...</tag>` schema in a single intentional snapshot diff. PR B
//! keeps every output byte-identical with PR A so the four golden
//! snapshots in `__snapshots__/` continue to pass without
//! `UPDATE_SNAPSHOTS=1`.
//!
//! [`SystemPromptBuilder`]: super::SystemPromptBuilder

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

pub use agents_md::{probe_agents_md, AgentsMdProbe};

#[cfg(test)]
pub(crate) use agents_md::{AGENTS_MD_MAX_BYTES, AGENTS_MD_SECTION_TAG_PREFIX};
