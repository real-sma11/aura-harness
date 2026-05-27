//! Shared retry classifier and budget for the streaming pump.
//!
//! Phase 1 of the reasoner-retry consolidation seated the canonical
//! retry envelope inside [`aura_config::reasoner()`] so the streaming
//! pump, the legacy buffered streaming retry loop
//! ([`crate::agent_loop::streaming::AgentLoop::stream_retry_params`]),
//! and `aura_reasoner::AnthropicConfig` all read the same
//! `AURA_LLM_MAX_RETRIES` / `AURA_LLM_BACKOFF_INITIAL_MS` /
//! `AURA_LLM_BACKOFF_CAP_MS` triple at startup.
//!
//! Phase 3 collapses the previously-duplicated `stream_retry_params`
//! helper in [`crate::agent_loop::streaming`] onto this one, so
//! operators tune both paths together and a single definition keeps
//! the budget invariant from drifting.

use aura_reasoner::PartialToolUse;

/// Retry budget / backoff envelope shared with the legacy buffered
/// streaming retry path and `aura_reasoner::AnthropicConfig`. Both
/// paths now read the same [`aura_config::reasoner().llm_retry`]
/// value (sourced once from `AURA_LLM_MAX_RETRIES` /
/// `AURA_LLM_BACKOFF_INITIAL_MS` / `AURA_LLM_BACKOFF_CAP_MS` at
/// startup) so operators tune both paths together.
pub(in crate::agent_loop) fn stream_retry_params() -> (u32, u64, u64) {
    aura_config::reasoner().llm_retry.as_legacy_triple()
}

/// Captured retry context for one in-flight stream that aborted with
/// a partial `tool_use`. The pump's retry loop seeds this from the
/// most recent `StreamAbortedWithPartial` so subsequent attempts can
/// surface meaningful telemetry (and the final `ToolCallFailed` event
/// names a real tool when the retry budget is exhausted).
pub(super) struct PartialRetryState {
    pub(super) tool_use_id: String,
    pub(super) tool_name: String,
    pub(super) reason: String,
}

/// Fold a fresh `StreamAbortedWithPartial` into the running retry
/// context. The `previous` state's identifiers are kept when the new
/// abort did not carry a [`PartialToolUse`] (i.e. the stream died
/// before `content_block_start` fired), so the eventual
/// `ToolCallFailed` still names the original tool when we have it.
pub(super) fn update_partial_retry_state(
    previous: Option<PartialRetryState>,
    reason: String,
    partial_tool_use: Option<PartialToolUse>,
) -> PartialRetryState {
    let (prev_id, prev_name) = previous.map_or_else(
        || ("<unknown>".to_string(), "<unknown>".to_string()),
        |state| (state.tool_use_id, state.tool_name),
    );
    let (tool_use_id, tool_name) = partial_tool_use.map_or((prev_id, prev_name), |partial| {
        (partial.tool_use_id, partial.tool_name)
    });
    PartialRetryState {
        tool_use_id,
        tool_name,
        reason,
    }
}
