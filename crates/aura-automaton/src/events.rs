use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Events emitted by an automaton during its lifecycle (start, stop, tool use, progress, etc.).
///
/// # `debug.*` frames
///
/// The `Debug*` variants below correspond 1:1 to
/// [`aura_agent::DebugEvent`] and intentionally serialize with an
/// explicit `type: "debug.<kind>"` tag (overriding the `snake_case`
/// default) so the `aura-os-server` run-log forwarder classifies them
/// into `llm_calls.jsonl`, `iterations.jsonl`, `blockers.jsonl`, or
/// `retries.jsonl`. The harness-side emitter is
/// [`aura_agent::AgentLoop`]; this enum is just the wire projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AutomatonEvent {
    // Lifecycle
    Started {
        automaton_id: String,
    },
    Stopped {
        automaton_id: String,
        reason: String,
    },
    Paused {
        automaton_id: String,
    },
    Resumed {
        automaton_id: String,
    },
    Error {
        automaton_id: String,
        message: String,
    },

    // Streaming / LLM
    TextDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(alias = "delta")]
        text: String,
    },
    ThinkingDelta {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(alias = "delta")]
        thinking: String,
    },
    Progress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        message: String,
    },

    // Tool usage
    #[serde(rename = "tool_use_start")]
    ToolCallStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        id: String,
        name: String,
    },
    ToolCallSnapshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        id: String,
        name: String,
        input: serde_json::Value,
        /// `true` when the forwarded JSON could not be parsed as a full
        /// value (e.g. the stream died mid-JSON during an in-flight
        /// `write_file`). Consumers can render a "streaming…" badge
        /// instead of an empty card. When the JSON parsed cleanly this
        /// is `false`. Defaults to `false` on the wire for backwards
        /// compatibility with pre-retry bundles.
        #[serde(default)]
        snapshot_partial: bool,
    },
    /// Authoritative "tool call finished" frame carrying the fully-
    /// parsed input alongside an error flag. Emitted by the harness for
    /// every `ToolResult`, in addition to the result itself, so the
    /// `aura-os-server` DoD gate can populate `files_changed` from
    /// successful `write_file`/`edit_file`/`delete_file` calls without
    /// having to stitch `tool_call_snapshot` and `tool_result` events
    /// together by id.
    ToolCallCompleted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        id: String,
        name: String,
        input: serde_json::Value,
        /// `false` on a successful tool call. Defaults to `false` on
        /// the wire for backwards-compatibility with older harnesses
        /// that emitted this variant without the field.
        #[serde(default)]
        is_error: bool,
    },
    ToolResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        id: String,
        name: String,
        result: String,
        is_error: bool,
    },

    // Dev loop specific
    TaskStarted {
        task_id: String,
        task_title: String,
    },
    TaskCompleted {
        task_id: String,
        summary: String,
    },
    TaskFailed {
        task_id: String,
        reason: String,
    },
    /// Emitted when a task completed but the per-task aggregate shows
    /// no file changes and no build/test/fmt/lint verification
    /// evidence. In that case the dev-loop / task-run builtins skip
    /// dispatching `git_commit` / `git_commit_push` to avoid creating
    /// orphan commits that the server-side DoD gate would later have
    /// to roll back.
    CommitSkipped {
        task_id: String,
        reason: String,
    },
    TaskRetrying {
        task_id: String,
        attempt: u32,
        reason: String,
    },
    /// Mid-stream `tool_use` request was interrupted and the agent is
    /// retrying with exponential backoff. 1:1 projection of
    /// [`aura_agent::AgentLoopEvent::ToolCallRetrying`]; the
    /// automaton forwarder attaches the task_id for correlation.
    ToolCallRetrying {
        task_id: String,
        tool_use_id: String,
        tool_name: String,
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        reason: String,
    },
    /// Retry budget for an in-flight `tool_use` exhausted. 1:1
    /// projection of [`aura_agent::AgentLoopEvent::ToolCallFailed`].
    ToolCallFailed {
        task_id: String,
        tool_use_id: String,
        tool_name: String,
        reason: String,
    },
    LoopFinished {
        outcome: String,
        completed_count: u32,
        failed_count: u32,
    },

    // Spec generation
    SpecSaved {
        spec_id: String,
        title: String,
    },
    SpecsTitle {
        title: String,
    },
    SpecsSummary {
        summary: String,
    },

    // Build / test
    BuildVerificationStarted,
    BuildVerificationPassed,
    BuildVerificationFailed {
        error_count: u32,
    },
    TestVerificationStarted,
    TestVerificationPassed,
    TestVerificationFailed {
        failure_count: u32,
    },
    BuildFixAttempt {
        attempt: u32,
        max_attempts: u32,
    },
    TestFixAttempt {
        attempt: u32,
        max_attempts: u32,
    },

    // File ops
    FileOpsApplied {
        files_written: u32,
        files_deleted: u32,
    },

    // Git
    GitCommitted {
        task_id: String,
        commit_sha: String,
    },
    GitCommitFailed {
        task_id: String,
        reason: String,
    },
    GitPushed {
        task_id: String,
        repo: String,
        branch: String,
        commits: Vec<serde_json::Value>,
    },
    GitPushFailed {
        task_id: String,
        reason: String,
    },

    // Session
    SessionRolledOver {
        old_session_id: String,
        new_session_id: String,
    },

    // Token usage
    TokenUsage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        input_tokens: u64,
        output_tokens: u64,
    },

    // Chat-specific
    MessageSaved {
        message_id: String,
    },
    AgentInstanceUpdated {
        instance_id: String,
    },

    // Generic
    LogLine {
        message: String,
    },
    Done,

    // ------------------------------------------------------------------
    // `debug.*` observability frames. The `rename` attributes force the
    // `type` tag to be `debug.llm_call` / `debug.iteration` /
    // `debug.blocker` / `debug.retry` so the aura-os forwarder routes
    // them into the matching per-run `*.jsonl` channel files.
    // ------------------------------------------------------------------
    #[serde(rename = "debug.llm_call")]
    DebugLlmCall {
        timestamp: DateTime<Utc>,
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_instance_id: Option<String>,
        /// HTTP `x-request-id` from the provider response. Correlation
        /// key against aura-router / provider logs. Accepts the legacy
        /// `request_id` field on the wire for backwards compatibility
        /// with bundles emitted before the split.
        #[serde(default, alias = "request_id", skip_serializing_if = "Option::is_none")]
        provider_request_id: Option<String>,
        /// Provider-internal message id (Anthropic
        /// `message_start.message.id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    #[serde(rename = "debug.iteration")]
    DebugIteration {
        timestamp: DateTime<Utc>,
        index: u32,
        tool_calls: u32,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    #[serde(rename = "debug.blocker")]
    DebugBlocker {
        timestamp: DateTime<Utc>,
        kind: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
    #[serde(rename = "debug.retry")]
    DebugRetry {
        timestamp: DateTime<Utc>,
        reason: String,
        attempt: u32,
        wait_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },
}

impl From<aura_agent::DebugEvent> for AutomatonEvent {
    fn from(ev: aura_agent::DebugEvent) -> Self {
        match ev {
            aura_agent::DebugEvent::LlmCall {
                timestamp,
                provider,
                model,
                input_tokens,
                output_tokens,
                duration_ms,
                task_id,
                agent_instance_id,
                provider_request_id,
                message_id,
            } => Self::DebugLlmCall {
                timestamp,
                provider,
                model,
                input_tokens,
                output_tokens,
                duration_ms,
                task_id,
                agent_instance_id,
                provider_request_id,
                message_id,
            },
            aura_agent::DebugEvent::Iteration {
                timestamp,
                index,
                tool_calls,
                duration_ms,
                task_id,
            } => Self::DebugIteration {
                timestamp,
                index,
                tool_calls,
                duration_ms,
                task_id,
            },
            aura_agent::DebugEvent::Blocker {
                timestamp,
                kind,
                path,
                message,
                task_id,
            } => Self::DebugBlocker {
                timestamp,
                kind,
                path,
                message,
                task_id,
            },
            aura_agent::DebugEvent::Retry {
                timestamp,
                reason,
                attempt,
                wait_ms,
                provider,
                model,
                task_id,
            } => Self::DebugRetry {
                timestamp,
                reason,
                attempt,
                wait_ms,
                provider,
                model,
                task_id,
            },
        }
    }
}
