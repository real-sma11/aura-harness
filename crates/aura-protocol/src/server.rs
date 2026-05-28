//! Outbound (server → client) wire messages and their payloads.
//!
//! [`OutboundMessage`] is the top-level enum streamed from the harness to a
//! websocket client. It covers session-level events (ready / start / end),
//! incremental text and tool deltas, tool-result and tool-approval prompts,
//! errors, and image / video / 3D generation events.

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

use crate::common::{ToolApprovalRemember, ToolStateWire};

/// Top-level outbound message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub enum OutboundMessage {
    /// Session initialized and ready.
    SessionReady(SessionReady),
    /// Start of an assistant message.
    AssistantMessageStart(AssistantMessageStart),
    /// Incremental text content from the model.
    TextDelta(TextDelta),
    /// Incremental thinking content from the model.
    ThinkingDelta(ThinkingDelta),
    /// A tool use has started.
    ToolUseStart(ToolUseStart),
    /// Snapshot of a tool call with accumulated input (streamed incrementally).
    ToolCallSnapshot(ToolCallSnapshot),
    /// Result of a tool execution.
    ToolResult(ToolResultMsg),
    /// Ask the client to approve or deny a live tool call.
    ToolApprovalPrompt(ToolApprovalPrompt),
    /// End of an assistant message (turn complete).
    ///
    /// Boxed because [`AssistantMessageEnd`] carries a full
    /// [`SessionUsage`] (including [`ContextBreakdown`]) plus a
    /// [`FilesChanged`] payload. Holding it inline would make every
    /// other variant 360+ bytes wide via Rust's union layout, even
    /// though most (`TextDelta`, `ToolUseStart`, `ProgressMsg`) are
    /// far smaller. Boxing isolates the cost on the rarer terminal
    /// frame.
    AssistantMessageEnd(Box<AssistantMessageEnd>),
    /// An error occurred.
    Error(ErrorMsg),
    /// Progress heartbeat. Strictly additive: the harness emits these
    /// during long tool calls (`stage = "tool_running"`, every
    /// `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS`) so the aura-os
    /// sliding-idle watchdog and the client-side stuck-stream
    /// watchdog see forward motion and don't trip a `turn_timeout` on
    /// a turn that is actually working. Older clients ignore unknown
    /// SSE event types (the chat handler already does — see
    /// `interface/src/hooks/use-chat-stream/build-stream-handler.ts`
    /// `EventType.Progress` branch) so adding the variant is
    /// wire-compatible in both directions.
    Progress(ProgressMsg),
    /// Generation started.
    GenerationStart(GenerationStart),
    /// Generation progress update.
    GenerationProgress(GenerationProgressMsg),
    /// Partial image data (progressive rendering).
    GenerationPartialImage(GenerationPartialImage),
    /// Generation completed successfully.
    GenerationCompleted(GenerationCompleted),
    /// Generation failed.
    GenerationError(GenerationErrorMsg),
}

/// Payload for `session_ready`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct SessionReady {
    pub session_id: String,
    pub tools: Vec<ToolInfo>,
    /// Skills that are active (installed + resolved) for this session's agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<SkillInfo>,
}

/// Minimal tool info for the `session_ready` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    #[serde(default = "default_tool_state_on")]
    pub effective_state: ToolStateWire,
}

const fn default_tool_state_on() -> ToolStateWire {
    ToolStateWire::On
}

/// Minimal skill info surfaced in `session_ready`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Payload for `assistant_message_start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AssistantMessageStart {
    pub message_id: String,
}

/// Payload for `text_delta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct TextDelta {
    pub text: String,
}

/// Payload for `thinking_delta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ThinkingDelta {
    pub thinking: String,
}

/// Payload for `tool_use_start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolUseStart {
    pub id: String,
    pub name: String,
}

/// Payload for `tool_call_snapshot` -- incrementally accumulated tool input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolCallSnapshot {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// Payload for `tool_result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolResultMsg {
    pub name: String,
    pub result: String,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// Payload for `tool_approval_prompt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolApprovalPrompt {
    pub request_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub agent_id: String,
    pub remember_options: Vec<ToolApprovalRemember>,
}

/// Payload for `assistant_message_end`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AssistantMessageEnd {
    pub message_id: String,
    pub stop_reason: String,
    pub usage: SessionUsage,
    pub files_changed: FilesChanged,
    /// Phase 5 billing roll-up: the originating user whose budget
    /// should absorb this turn's cost. When `None`, the immediate
    /// agent owner is billed (today's behavior). Populated by the
    /// harness when a spawned agent's work should roll up to the
    /// ancestor user via `walk_parent_chain`. Strictly additive —
    /// older harness builds never set this field; older clients
    /// ignore it on deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_id: Option<String>,
}

/// Token usage information for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct SessionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_context_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cumulative_input_tokens: u64,
    pub cumulative_output_tokens: u64,
    pub cumulative_cache_creation_input_tokens: u64,
    pub cumulative_cache_read_input_tokens: u64,
    /// Fraction of the model's context window consumed (0.0–1.0).
    pub context_utilization: f32,
    /// Model identifier used for this turn.
    pub model: String,
    /// Provider name (e.g., "anthropic").
    pub provider: String,
    /// Per-bucket token estimates that sum (approximately) to
    /// `estimated_context_tokens`. Strictly additive — older harness
    /// builds emit `ContextBreakdown::default()` (all zeros), and the
    /// frontend treats an all-zero breakdown as "not available" and
    /// falls back to the legacy used/total view.
    #[serde(default)]
    pub context_breakdown: ContextBreakdown,
}

/// Per-bucket token estimates for the current session context, computed
/// using the same `chars / CHARS_PER_TOKEN` heuristic as
/// [`SessionUsage::estimated_context_tokens`]. The buckets approximate
/// what the model actually receives on the next turn:
///
/// - `system_prompt_tokens` — the rendered system prompt.
/// - `tools_tokens` — serialized tool definitions (name + description +
///   JSON schema for each tool the request would carry).
/// - `skills_tokens` — installed skill metadata (name + description).
/// - `mcp_tokens` — reserved for MCP server context once aura-harness
///   gains MCP support; today this is always `0`.
/// - `subagents_tokens` — registered subagent kind specs.
/// - `conversation_tokens` — the live message transcript including
///   tool results and assistant turns. This is the same number as
///   `estimated_context_tokens` minus the static buckets above.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ContextBreakdown {
    pub system_prompt_tokens: u64,
    pub tools_tokens: u64,
    pub skills_tokens: u64,
    pub mcp_tokens: u64,
    pub subagents_tokens: u64,
    pub conversation_tokens: u64,
    /// Tokens served from the upstream provider's prompt cache during
    /// the most recent turn (Anthropic's `cache_read_input_tokens` or
    /// OpenAI's `prompt_tokens_details.cached_tokens`). Describes what
    /// fraction of the *conversation* bucket was a cache hit; not a
    /// separate context bucket, so excluded from [`Self::total`] and
    /// [`Self::is_empty`].
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Tokens written to the upstream provider's prompt cache during
    /// the most recent turn (Anthropic's `cache_creation_input_tokens`,
    /// or the cache-miss portion of OpenAI's responses). See
    /// [`Self::cache_read_tokens`] for why this is NOT included in
    /// [`Self::total`] / [`Self::is_empty`].
    #[serde(default)]
    pub cache_creation_tokens: u64,
}

impl ContextBreakdown {
    /// True when every bucket is zero. Used by the frontend to detect
    /// pre-upgrade harness builds and fall back to the legacy popover.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.system_prompt_tokens == 0
            && self.tools_tokens == 0
            && self.skills_tokens == 0
            && self.mcp_tokens == 0
            && self.subagents_tokens == 0
            && self.conversation_tokens == 0
    }

    /// Sum of every bucket. Useful as a sanity check against
    /// [`SessionUsage::estimated_context_tokens`].
    #[must_use]
    pub fn total(&self) -> u64 {
        self.system_prompt_tokens
            .saturating_add(self.tools_tokens)
            .saturating_add(self.skills_tokens)
            .saturating_add(self.mcp_tokens)
            .saturating_add(self.subagents_tokens)
            .saturating_add(self.conversation_tokens)
    }
}

/// A single file mutation observed during a turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct FileOp {
    pub path: String,
    pub operation: String,
}

/// Per-file diff metadata that runs alongside the path lists in
/// [`FilesChanged`]. Optional and additive: senders that don't compute
/// line counts (older harnesses, write/delete tool paths) simply omit
/// the entry, and consumers must treat "no entry for path" / "0 / 0"
/// as "unknown", not "no change".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct FileDiff {
    pub path: String,
    pub lines_added: u32,
    pub lines_removed: u32,
}

/// Summary of file mutations during a turn.
///
/// `diffs` is a parallel list of per-path line-count metadata. It lives
/// alongside `created` / `modified` / `deleted` (rather than replacing
/// them) so older clients that only know about the path lists keep
/// working untouched. The serde default + skip_if_empty pair keeps the
/// wire format byte-identical when no diffs are computed, preserving
/// JSON-shape compatibility with any pinned schemas.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct FilesChanged {
    pub created: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffs: Vec<FileDiff>,
}

impl FilesChanged {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.created.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

/// Payload for `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ErrorMsg {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
    /// Short opaque id (12 lowercase hex chars) used to correlate this
    /// error across server logs, client breadcrumbs, and user-pasted
    /// support reports. Strictly additive on the wire:
    ///
    /// - Older clients that don't know the field deserialize fine
    ///   (`#[serde(default)]` keeps the field optional inbound), and
    ///   the sender omits it from the JSON when `None`
    ///   (`skip_serializing_if = "Option::is_none"`) so older receivers
    ///   never see an unexpected key.
    /// - Older harness builds simply leave it `None`. The aura-os SSE
    ///   remap boundary in `apps/aura-os-server/src/handlers/agents/chat/errors.rs`
    ///   keeps stamping a `(support_id=<id>)` suffix into `message`
    ///   for that case so existing clients still get a usable id.
    /// - Newer harness in-process emit sites (e.g. the watchdog
    ///   `stream_stalled` / `turn_timeout` synth, the agent loop's
    ///   `agent_stalled` terminal event) can pre-populate the field so
    ///   the same id appears on both the structured field and the
    ///   message suffix; the SSE remap path leaves a prepopulated id
    ///   untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_id: Option<String>,
}

/// Payload for `progress` heartbeat / status events.
///
/// `stage` is the only required field and carries a free-form short
/// label (`"tool_running"`, `"lagged"`, `"forked_for_context"`, …).
/// Unknown stages flow straight through to the client which renders
/// them as a generic progress label, so we can introduce new stages
/// without coordinating a wire bump.
///
/// Optional fields are omitted from the JSON when `None`
/// (`skip_serializing_if = "Option::is_none"`) and default to `None`
/// inbound (`#[serde(default)]`) so older harness/client pairs that
/// don't know about them deserialize cleanly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ProgressMsg {
    /// Short machine-readable stage tag. The aura-os chat client
    /// renders unknown values as the literal label, so adding new
    /// stages does not require a coordinated client release.
    pub stage: String,
    /// Tool whose long-running execution is producing this heartbeat.
    /// Set on `stage = "tool_running"`; left `None` for stages that
    /// don't refer to a single tool (e.g. `"lagged"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// Wall-clock milliseconds since the heartbeat's reference event
    /// (tool start for `"tool_running"`). Optional — older clients
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// Optional human-readable label / detail string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// ============================================================================
// Generation Event Types
// ============================================================================

/// Payload for `generation_start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationStart {
    pub mode: String,
}

/// Payload for `generation_progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationProgressMsg {
    pub percent: f64,
    pub message: String,
}

/// Payload for `generation_partial_image`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationPartialImage {
    pub data: String,
}

/// Payload for `generation_completed`.
///
/// The `payload` field carries the raw response from the generation backend,
/// whose shape varies by mode (image vs 3D).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationCompleted {
    pub mode: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Payload for `generation_error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationErrorMsg {
    pub code: String,
    pub message: String,
}
