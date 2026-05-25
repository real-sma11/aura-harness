//! Prompt builders for the agent execution, fix, and chat flows.
//!
//! All builders accept lightweight descriptor structs instead of domain objects,
//! keeping this module free of app-layer dependencies.

// `auxiliary` (not `aux`) because Windows reserves `AUX` as a device name
// and refuses to create directories with that name in any case form.
pub mod auxiliary;
mod context;
pub mod enrichment;
mod fix;
pub mod steering;
mod system;
mod turn_kernel_system;

#[cfg(test)]
mod guardrail_tests;

pub use crate::verify::error_types::{parse_error_references, BuildFixAttemptRecord};
pub use context::build_agentic_task_context;
pub use enrichment::{
    default_caps, extract_hints, resolve_hints, ContextHints, FsWorkspace, ResolveCaps,
    ResolvedContext, SymbolHit, WorkspaceReader,
};
pub use fix::{build_fix_prompt_with_history, BuildFixPromptParams};
pub use steering::{SteeringInjector, SteeringKind};
pub use system::{
    agentic_execution_system_prompt, build_chat_system_prompt, probe_agents_md, AgentsMdProbe,
    SystemPromptBuilder, CHAT_SYSTEM_PROMPT_BASE,
};
pub use turn_kernel_system::default_system_prompt;

/// Minimal project descriptor for prompt builders.
#[derive(Debug)]
pub struct ProjectInfo<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub folder_path: &'a str,
    pub build_command: Option<&'a str>,
    pub test_command: Option<&'a str>,
}

/// Borrowed agent-identity fields consumed by
/// [`SystemPromptBuilder::agent_identity`]. The owning representation
/// on the wire is [`aura_protocol::AgentIdentityWire`]; the
/// `aura-runtime` automaton bridge converts wire → borrowed at the
/// kickoff site so `aura-agent` stays free of protocol dependencies.
#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    pub name: &'a str,
    pub role: &'a str,
    pub personality: &'a str,
}

/// Bundle of identity / skills / operator-authored prompt threaded
/// from the wire layer into [`AgenticTaskParams::agent`].
///
/// Re-introduced in PR B (PR A removed the unused `agent` field
/// alongside the dead `build_agent_preamble` helper). Today every
/// `AgenticTaskParams` construction site passes `None`; PR C populates
/// the wire fields on the `aura-os` side and lets `Some(_)` flow
/// through to the builder.
///
/// [`AgenticTaskParams::agent`]: crate::agent_runner::AgenticTaskParams::agent
#[derive(Debug, Clone, Copy)]
pub struct AgentInfo<'a> {
    pub identity: Option<AgentIdentity<'a>>,
    pub skills: &'a [String],
    pub system_prompt: Option<&'a str>,
}

/// Minimal spec descriptor.
#[derive(Debug)]
pub struct SpecInfo<'a> {
    pub title: &'a str,
    pub markdown_contents: &'a str,
}

/// Minimal task descriptor.
#[derive(Debug)]
pub struct TaskInfo<'a> {
    pub title: &'a str,
    pub description: &'a str,
    pub execution_notes: &'a str,
    pub files_changed: &'a [FileChangeEntry],
}

/// A single file-change entry (path + operation label).
#[derive(Debug, Clone)]
pub struct FileChangeEntry {
    pub path: String,
    pub op: String,
}

/// Minimal session descriptor.
#[derive(Debug)]
pub struct SessionInfo<'a> {
    pub summary_of_previous_context: &'a str,
}
