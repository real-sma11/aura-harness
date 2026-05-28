//! Borrowed view structs threaded into the prompt builders.
//!
//! These descriptors are intentionally minimal: each one is the
//! smallest set of fields a builder needs, kept as `&str` borrows so
//! the prompt layer stays free of agent-/runtime-layer domain types.
//! Conversions from heavyweight `aura-core` / `aura-protocol` types
//! happen at the `aura-agent` boundary.
//!
//! Phase 0 rules debt: every public field below now documents its
//! purpose; the pre-Phase-2 versions in `aura-agent/src/prompts/mod.rs`
//! were largely undocumented.

/// Minimal project descriptor for prompt builders.
///
/// `project_id` is `Option<&str>` so the dev-loop path can keep
/// passing `None` (the dev-loop's `<project_context>` has never
/// surfaced a `project_id` line, and the four base dev-loop snapshots
/// assert that). The chat WS migration sets it to `Some(...)` so the
/// chat `<project_context>` retains the `project_id:` line the legacy
/// `aura-os` helper used to surface for tool-id grounding.
#[derive(Debug)]
pub struct ProjectInfo<'a> {
    /// Optional project identifier. When present, it is rendered as
    /// the leading `project_id:` line of the `<project_context>` block.
    pub project_id: Option<&'a str>,
    /// Human-friendly project name. Always rendered.
    pub name: &'a str,
    /// Free-form description; omitted from the rendered block when blank.
    pub description: &'a str,
    /// Absolute path to the project's working directory. Always rendered.
    pub folder_path: &'a str,
    /// Build command (e.g. `cargo build`); omitted when `None` /
    /// blank. Spliced verbatim into the dev-loop workflow block.
    pub build_command: Option<&'a str>,
    /// Test command (e.g. `cargo test`); omitted when `None` / blank.
    /// Spliced verbatim into the dev-loop workflow block; the
    /// `task_done` DoD gate uses the same string.
    pub test_command: Option<&'a str>,
}

/// Borrowed agent-identity fields consumed by
/// [`crate::system::SystemPromptBuilder::agent_identity`]. The owning
/// representation on the wire is `aura_protocol::AgentIdentityWire`;
/// the `aura-runtime` automaton bridge converts wire → borrowed at
/// the kickoff site so this crate stays free of protocol dependencies.
#[derive(Debug, Clone, Copy)]
pub struct AgentIdentity<'a> {
    /// Display name of the agent (e.g. `"Aura Backend"`); omitted from
    /// the rendered block when blank.
    pub name: &'a str,
    /// Short role tag (e.g. `"Senior backend engineer"`); omitted when
    /// blank.
    pub role: &'a str,
    /// Personality / tone descriptor; omitted when blank.
    pub personality: &'a str,
}

/// Bundle of identity / skills / operator-authored prompt threaded
/// from the wire layer into the agent runner's task params.
///
/// PR B re-introduced the field after PR A removed the unused
/// `agent` field alongside the dead `build_agent_preamble` helper.
/// Today every `AgenticTaskParams` construction site passes `None`;
/// PR C populates the wire fields on the `aura-os` side and lets
/// `Some(_)` flow through to the builder.
#[derive(Debug, Clone, Copy)]
pub struct AgentInfo<'a> {
    /// Identity, when populated by the wire layer.
    pub identity: Option<AgentIdentity<'a>>,
    /// Operator-defined skill labels. Empty slice drops the
    /// `<agent_skills>` block from the assembled prompt.
    pub skills: &'a [String],
    /// Operator-authored system prompt prepended verbatim. `None` /
    /// blank drops the `<agent_system_prompt>` block.
    pub system_prompt: Option<&'a str>,
}

/// Minimal spec descriptor for the bootstrap user-message block.
#[derive(Debug)]
pub struct SpecInfo<'a> {
    /// Spec title, rendered as the `# Spec:` header.
    pub title: &'a str,
    /// Markdown body of the spec; truncated to
    /// [`aura_config::PromptsConfig::bootstrap_spec_bytes`] before
    /// injection.
    pub markdown_contents: &'a str,
}

/// Minimal task descriptor for the bootstrap user-message block and
/// the fix-prompt header.
#[derive(Debug)]
pub struct TaskInfo<'a> {
    /// Task title, rendered as the `# Task:` header.
    pub title: &'a str,
    /// Free-form task description threaded into the bootstrap user
    /// message verbatim.
    pub description: &'a str,
    /// Notes accumulated by the harness across previous attempts on
    /// the same task; rendered under `# Notes from Prior Attempts`
    /// when non-empty.
    pub execution_notes: &'a str,
    /// File-change records produced by the agent in earlier attempts
    /// (used to summarize completed predecessors).
    pub files_changed: &'a [FileChangeEntry],
}

/// A single file-change entry (path + operation label) used by the
/// completed-predecessor summary in the bootstrap context.
#[derive(Debug, Clone)]
pub struct FileChangeEntry {
    /// Workspace-relative path of the changed file.
    pub path: String,
    /// Short label for the operation (`"create"`, `"modify"`, `"delete"`).
    pub op: String,
}

/// Minimal session descriptor; carries the cross-iteration summary
/// from the previous turn into the bootstrap user-message block.
#[derive(Debug)]
pub struct SessionInfo<'a> {
    /// Optional summary of the previous transcript. Rendered under
    /// `# Previous Context Summary` when non-empty.
    pub summary_of_previous_context: &'a str,
}
