//! Streaming events emitted during agent execution.
//!
//! [`TurnEvent`] is the in-process event carried over the
//! `mpsc::Sender<TurnEvent>` channel that orchestrators pass into the
//! agent loop. Wire-only frames (`debug.*` observability events) live
//! in [`super::wire`] and surface here through the [`TurnEvent::Debug`]
//! variant so a single channel preserves arrival order between UI and
//! observability traffic.

use super::wire::DebugEvent;

/// Unified events emitted during agent/turn execution.
///
/// Covers all events emitted during agent execution.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Incremental text content from the model.
    TextDelta(String),

    /// Incremental thinking/reasoning content from the model.
    ThinkingDelta(String),

    /// Thinking block completed (end of extended-thinking content).
    ThinkingComplete,

    /// A tool use block started streaming.
    ToolStart {
        /// Tool use ID from the model.
        id: String,
        /// Tool name.
        name: String,
    },

    /// Incremental snapshot of tool input JSON as it streams in.
    ToolInputSnapshot {
        /// Tool use ID.
        id: String,
        /// Tool name.
        name: String,
        /// Accumulated input JSON so far (may be partial/incomplete).
        input: String,
    },

    /// A tool execution completed (with full result).
    ToolComplete {
        /// Tool name.
        name: String,
        /// Tool arguments (JSON), if available.
        args: Option<serde_json::Value>,
        /// Result content (text).
        result: String,
        /// Whether the tool execution failed.
        is_error: bool,
    },

    /// Tool result that will be appended to context.
    ToolResult {
        /// Tool use ID.
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Result content.
        content: String,
        /// Whether the result is an error.
        is_error: bool,
        /// Optional rendered image (base64 + media type) produced by
        /// computer-use / vision tools. `None` for text-only tools.
        /// Carried to the wire boundary so aura-os can persist + replay
        /// the screenshot to the model.
        image: Option<aura_core_types::ToolResultImage>,
    },

    /// Authoritative "this tool call finished" marker carrying the full
    /// parsed input alongside the outcome flag.
    ///
    /// Emitted alongside `ToolResult` at the moment the tool call
    /// returns. Unlike `ToolInputSnapshot` (partial JSON during
    /// streaming) and `ToolResult` (outcome text without input), this
    /// event is the single frame that carries BOTH the fully-parsed
    /// `input` AND whether the call succeeded. Downstream forwarders
    /// (in particular the `aura-os-server` dev-loop gate) rely on this
    /// to populate `files_changed` from successful
    /// `write_file`/`edit_file`/`delete_file` calls without having to
    /// stitch snapshot + result events together.
    ToolCallCompleted {
        /// Tool use ID (matches the preceding `ToolStart` /
        /// `ToolInputSnapshot` / `ToolResult` events).
        tool_use_id: String,
        /// Tool name.
        tool_name: String,
        /// Fully-parsed tool input JSON (never partial).
        input: serde_json::Value,
        /// Whether the call returned an error.
        is_error: bool,
    },

    /// One iteration (model call + tool execution) completed.
    IterationComplete {
        /// Zero-based iteration index.
        iteration: usize,
        /// Input tokens used in this iteration.
        input_tokens: u64,
        /// Output tokens used in this iteration.
        output_tokens: u64,
    },

    /// Streaming is complete for the current step.
    StepComplete,

    /// Streaming was interrupted and restarted. Consumers must discard
    /// any buffered partial content for the current iteration and treat
    /// subsequent events as the authoritative source.
    StreamReset {
        /// Human-readable reason for the reset.
        reason: String,
    },

    /// An in-flight `tool_use` streaming request was interrupted by a
    /// transient provider error and will be retried with exponential
    /// backoff. Emitted BEFORE the backoff sleep so the UI can render
    /// "Write retrying (attempt/max)..." instead of "Write failed".
    ///
    /// When the in-flight tool use is not recoverable from the
    /// accumulator (e.g. the stream died before `content_block_start`)
    /// `tool_use_id` and `tool_name` are the placeholder string
    /// `"<unknown>"`.
    ToolCallRetrying {
        /// Provider-side `tool_use` id, or `"<unknown>"`.
        tool_use_id: String,
        /// Tool name (`write_file`, `edit_file`, ...), or `"<unknown>"`.
        tool_name: String,
        /// 1-based attempt number that is about to run.
        attempt: u32,
        /// Total retry budget.
        max_attempts: u32,
        /// Backoff the loop is about to sleep, in milliseconds.
        delay_ms: u64,
        /// Human-readable classification (already prefixed with the
        /// upstream error-type when available).
        reason: String,
    },

    /// Retry budget for an in-flight `tool_use` streaming request was
    /// exhausted; the interrupted call is abandoned. The outer task-
    /// level retry ladder (in `aura-os-server`) takes over from here.
    ToolCallFailed {
        /// Provider-side `tool_use` id, or `"<unknown>"`.
        tool_use_id: String,
        /// Tool name, or `"<unknown>"`.
        tool_name: String,
        /// Human-readable classification of the final error.
        reason: String,
    },

    /// Best-effort test-suite outcome surfaced after `task_done`.
    ///
    /// Codex-parity (May 2026): the harness no longer hard-gates
    /// completions on the project test suite. The suite is still run
    /// once on `task_done` so the operator/UI sees whether it passed,
    /// but a failing run no longer blocks the task — it is surfaced as
    /// this warning instead. `task_executor::handle_task_done` emits
    /// it before returning the success tool_result.
    TestSuiteWarning {
        /// `true` when the project test suite reported success.
        passed: bool,
        /// Short human-readable summary line (e.g. `"9 passed, 1 failed"`).
        summary: String,
        /// Names of failing tests as reported by the test runner.
        failed_tests: Vec<String>,
    },

    /// A warning was injected into the context.
    Warning(String),

    /// An error occurred during execution.
    Error {
        /// Machine-readable error code.
        code: String,
        /// Human-readable description.
        message: String,
        /// Whether the loop can continue after this error.
        recoverable: bool,
    },

    /// Heartbeat or status event the harness wants to surface to the
    /// client without it counting as text/tool/error traffic.
    ///
    /// Phase 6 of agent-stuck-and-reset: long tool calls emit
    /// `Progress { stage: "tool_running", tool_name, elapsed_ms }`
    /// every `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS` so aura-os's
    /// sliding-idle watchdog (which would otherwise trip
    /// `turn_timeout` after 180s of broadcast quiet) and the
    /// client-side stuck-stream watchdog see forward motion. The
    /// optional fields are deliberately untyped beyond the wire
    /// shape; callers populate whatever makes sense for the stage.
    Progress {
        /// Short, machine-readable stage tag (e.g. `"tool_running"`).
        /// The aura-os chat client renders unknown stages as the
        /// literal label, so adding new tags does not require a
        /// coordinated client release.
        stage: String,
        /// Tool whose long-running execution is producing this
        /// heartbeat. Set on `stage = "tool_running"`; left `None`
        /// for stages that don't refer to a single tool.
        tool_name: Option<String>,
        /// Wall-clock milliseconds since the heartbeat's reference
        /// event (tool start for `"tool_running"`).
        elapsed_ms: Option<u64>,
        /// Optional human-readable label / detail string.
        message: Option<String>,
    },

    /// Structured observability frame for the `aura-os` run bundle.
    /// Flows through the same channel as the UI-facing variants so
    /// that downstream forwarders preserve ordering.
    Debug(DebugEvent),
}

/// Backward-compatible alias. Prefer [`TurnEvent`] for new code.
pub type AgentLoopEvent = TurnEvent;
