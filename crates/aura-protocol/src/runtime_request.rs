//! Canonical wire shape for `POST /v1/run`.
//!
//! [`RuntimeRequest`] replaces the previous twin shapes
//! `SessionInit` (chat WS first-frame) + `AutomatonStartRequest`
//! (`POST /automaton/start` body) with a single discriminated-union
//! body. The harness `aura-runtime` gateway and any external consumer
//! (e.g. `aura-os`) both speak this shape.
//!
//! High-level grouping (field-ownership is intentional — each
//! sub-struct maps to exactly one downstream consumer):
//!
//! - [`RuntimeRequestType`]: discriminated union over the three run
//!   kinds the harness supports (`Chat`, `DevLoop`, `TaskRun`).
//! - [`AgentIdentity`]: "who is this agent" — template id, partition
//!   id, persona, skills, system prompt.
//! - [`ModelSelection`]: "what model to drive the agent with".
//! - [`WorkspaceLocation`]: "where the agent runs" (workspace + project
//!   path + git repo/branch).
//! - [`ProjectContext`]: "which project + which billing partition".
//! - [`AgentCapabilities`]: "what tools / integrations / intent
//!   classifier the agent can use".
//! - [`crate::AgentPermissionsWire`] + [`crate::AgentToolPermissionsWire`]:
//!   "what the agent is **allowed** to do" (kernel-enforced).

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

use crate::agent_identity::AgentPersona;
use crate::chat_project_info::ChatProjectInfoWire;
use crate::client::{ConversationMessage, IntentClassifierSpec, SessionModelOverrides};
use crate::installed::{InstalledIntegration, InstalledTool};
use crate::permissions::{AgentPermissionsWire, AgentToolPermissionsWire};

/// Canonical body of `POST /v1/run`.
///
/// Returned synchronously with `{ run_id, event_stream_url }`. The
/// caller then opens `WS /stream/:run_id` to receive events (and, on
/// the [`RuntimeRequestType::Chat`] variant, to send `user_message`
/// frames).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct RuntimeRequest {
    /// Discriminated union carrying the data unique to each request
    /// type. Renamed `r#type` so the wire payload uses the natural
    /// `"type"` key while Rust still gets a typed enum match.
    #[serde(rename = "type")]
    pub r#type: RuntimeRequestType,

    /// Who is this agent — template + partition + persona + skills +
    /// system prompt. See [`AgentIdentity`].
    pub agent_identity: AgentIdentity,

    /// What model to drive the agent with: id, max_tokens, max_turns,
    /// temperature, provider_overrides.
    pub model: ModelSelection,

    /// Where the agent runs: workspace path, project path, git
    /// repo/branch.
    pub workspace: WorkspaceLocation,

    /// Project context: project_id, typed project_info, billing
    /// header values (`aura_org_id`, `aura_session_id`,
    /// `aura_agent_id`). `None` only for callers that have no project
    /// (e.g. ad-hoc chat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectContext>,

    /// Policy bundle — what the agent is **allowed** to do.
    /// Capability + scope grants enforced by the kernel policy gate.
    #[serde(default)]
    pub agent_permissions: AgentPermissionsWire,

    /// Per-tool on/off overrides layered on top of
    /// [`Self::agent_permissions`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_permissions: Option<AgentToolPermissionsWire>,

    /// Runtime tools / integrations / intent classifier the agent
    /// **can use**. Distinct from [`Self::agent_permissions`]:
    /// permissions decide whether a capability is allowed;
    /// capabilities decide what concrete tools materialize it.
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,

    /// Bearer JWT forwarded to the model proxy + domain API calls.
    /// `None` is valid in dev (auth disabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_jwt: Option<String>,

    /// Originating end-user id for resolving + persisting tool
    /// defaults.
    pub user_id: String,
}

/// Discriminated union carrying the data unique to each run type.
///
/// One proper Rust enum — no separate `kind` discriminator + opaque
/// `kind_params` payload. The wire format uses
/// `serde(tag = "kind", content = "params", rename_all = "snake_case")`
/// so a `Chat` variant serializes as
/// `{"kind": "chat", "params": { conversation_messages: [...] }}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
#[serde(tag = "kind", content = "params", rename_all = "snake_case")]
pub enum RuntimeRequestType {
    /// Bidirectional chat session. The WS stream stays open after
    /// init and the client sends `user_message` frames over it.
    Chat {
        /// Prior conversation messages to hydrate into session
        /// history (empty for a brand-new session).
        #[serde(default)]
        conversation_messages: Vec<ConversationMessage>,
    },
    /// Dev-loop automaton — long-running, no client messages after
    /// kickoff. The variant is preserved so the type system signals
    /// intent ("this is a dev loop, not a chat or task") even when
    /// the union of common fields is sufficient.
    DevLoop {},
    /// Single-task automaton — runs one task to completion, then
    /// exits.
    TaskRun {
        /// Task UUID the automaton should execute.
        task_id: String,
        /// Retry warm-up: the reason text persisted on the previous
        /// attempt's `task_failed` record, threaded into
        /// `TaskInfo::execution_notes`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prior_failure: Option<String>,
        /// Retry warm-up: recent work-log entries the agent should
        /// re-see.
        #[serde(default)]
        work_log: Vec<String>,
    },
}

/// "Who is this agent" bundle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AgentIdentity {
    /// Stable template agent UUID — the row in the `agents` table.
    /// Used by the harness for skill / billing / permissions lookup
    /// keyed on the template (not the partition).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template_id: Option<String>,
    /// Partitioned harness agent id, one of:
    /// - `{template}::default`        (bare agent, no instance/session axis)
    /// - `{template}::{instance}`     (per-instance partition)
    /// - `{template}::{instance}::{session}` (per-(instance, session) partition)
    ///
    /// Used as the kernel turn-lock key + record-log partition;
    /// absent ⇒ the harness mints a fresh `AgentId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_id: Option<String>,
    /// Persona fields rendered into the `<agent_identity>` section
    /// of the assembled system prompt: name / role / personality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<AgentPersona>,
    /// Operator-curated skill names rendered as `<agent_skills>` in
    /// the assembled system prompt. Empty ⇒ no skills section.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Operator-authored system prompt (the "system prompt"
    /// textarea on the agent template). Rendered as
    /// `<agent_system_prompt>`. `None` or empty ⇒ no section
    /// rendered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

/// "What model to drive the agent with."
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ModelSelection {
    /// Model identifier (e.g. `"claude-opus-4-7"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Maximum tokens per model response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Maximum agentic steps per turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// User-selected reasoning-effort tier
    /// (`low`/`medium`/`high`/`xhigh`/`max`) from the chat model
    /// picker's thinking-level flyout. Parsed into
    /// `aura_reasoner::ThinkingEffort` and hard-pinned across the turn.
    /// Absent for models without effort tiers and for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Optional per-session model overrides applied on top of the
    /// harness's env-default router config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_overrides: Option<SessionModelOverrides>,
}

/// "Where the agent runs."
///
/// `workspace` is the sandboxed directory under the harness's
/// `workspaces` base. `project_path` is the real project directory
/// on the host filesystem; when set, tool execution happens directly
/// against this directory instead of the sandbox.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct WorkspaceLocation {
    /// Workspace directory path (must be under the server's
    /// `workspaces` base).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Absolute path to the real project directory on the host
    /// filesystem.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// Optional remote-git source URL for dev-loop / task-run
    /// kickoffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_repo_url: Option<String>,
    /// Optional remote-git branch paired with [`Self::git_repo_url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
}

/// "Which project + which billing partition."
///
/// `None` on a [`RuntimeRequest`] means "no project" (ad-hoc chat).
/// `Some(...)` carries the project UUID, the optional typed
/// project descriptor consumed by the harness's
/// `SystemPromptBuilder.project_context()`, and the billing-header
/// values (`X-Aura-Org-Id`, `X-Aura-Session-Id`, `X-Aura-Agent-Id`)
/// the harness stamps on outbound `/v1/messages` calls.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ProjectContext {
    /// Project UUID for domain tool calls (specs, tasks, etc.).
    pub project_id: String,
    /// Typed project descriptor surfaced into the chat-path system
    /// prompt's `<project_context>` section. `None` ⇒ no project
    /// block rendered (bare-agent chat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_info: Option<ChatProjectInfoWire>,
    /// Organization UUID for `X-Aura-Org-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_org_id: Option<String>,
    /// Storage session UUID for `X-Aura-Session-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_session_id: Option<String>,
    /// Project-agent UUID for `X-Aura-Agent-Id` billing header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aura_agent_id: Option<String>,
}

/// "What tools / integrations / intent classifier the agent can use."
///
/// Renamed from `capabilities` so the noun is qualified — a bare
/// `capabilities` would conflict visually with the
/// `aura_core_permissions::Capability` privilege enum.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AgentCapabilities {
    /// Installed tools registered for this run.
    #[serde(default)]
    pub installed_tools: Vec<InstalledTool>,
    /// Installed integrations authorized for this run.
    #[serde(default)]
    pub installed_integrations: Vec<InstalledIntegration>,
    /// Optional keyword-driven intent classifier spec. When present
    /// the harness narrows the per-turn tool surface based on each
    /// user message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_classifier: Option<IntentClassifierSpec>,
}

/// Response body of `POST /v1/run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct RuntimeRunResponse {
    /// Stable identifier for the spawned run. Used as the path
    /// segment on `WS /stream/:run_id` and on the lifecycle
    /// endpoints (`/v1/run/:id/status`, `/v1/run/:id/pause`,
    /// `/v1/run/:id/stop`).
    pub run_id: String,
    /// Convenience field — the relative WS path the client should
    /// open. Always `/stream/:run_id`; surfaced explicitly so older
    /// clients don't have to know the path scheme.
    pub event_stream_url: String,
}
