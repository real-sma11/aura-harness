//! Per-iteration logic: LLM calls, response accumulation, and stop-reason handling.

use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, ToolResultContent,
};
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::compaction;
use crate::constants::{
    CHARS_PER_TOKEN, NARRATION_TOKEN_HARD_BUDGET, NARRATION_TOKEN_SOFT_BUDGET,
    WRITE_FILE_CHUNK_BYTES,
};
use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::sanitize;
use crate::types::AgentLoopResult;

use super::streaming;
use super::{AgentLoop, AgentLoopConfig, LoopState};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// LLM call error handling
// ---------------------------------------------------------------------------

/// Describes why an LLM call failed, allowing the main loop to break cleanly.
pub(super) enum LlmCallError {
    InsufficientCredits(String),
    PromptTooLong(String),
    /// 429/529 surfaced by the provider. The message already includes the
    /// upstream `retry after N seconds` hint when one was reported so the
    /// UI can show a meaningful wait time. Emitted as `code: "rate_limit"`
    /// so clients can react (e.g. surface a retry affordance) rather than
    /// treat it as a generic fatal LLM error.
    RateLimited(String),
    Fatal(String),
}

impl LlmCallError {
    pub(super) fn apply(
        self,
        result: &mut AgentLoopResult,
        event_tx: Option<&Sender<AgentLoopEvent>>,
    ) {
        match self {
            Self::InsufficientCredits(msg) => {
                result.insufficient_credits = true;
                warn!("Insufficient credits (402), stopping loop");
                streaming::emit(
                    event_tx,
                    AgentLoopEvent::Error {
                        code: "insufficient_credits".to_string(),
                        message: msg,
                        recoverable: false,
                    },
                );
            }
            Self::RateLimited(msg) => {
                warn!(message = %msg, "LLM rate limited after retries, stopping loop");
                streaming::emit(
                    event_tx,
                    AgentLoopEvent::Error {
                        code: "rate_limit".to_string(),
                        message: msg.clone(),
                        // Retries already happened at the provider layer; the
                        // loop cannot recover this turn, but the next user
                        // turn (or a client-side retry) can succeed.
                        recoverable: true,
                    },
                );
                result.llm_error = Some(msg);
            }
            Self::PromptTooLong(msg) | Self::Fatal(msg) => {
                streaming::emit(
                    event_tx,
                    AgentLoopEvent::Error {
                        code: "llm_error".to_string(),
                        message: msg.clone(),
                        recoverable: false,
                    },
                );
                result.llm_error = Some(msg);
            }
        }
    }
}

impl LlmCallError {
    /// Convert a structured [`aura_reasoner::ReasonerError`] into an
    /// [`LlmCallError`] with the same credit/context/fatal classification
    /// the loop already applies to non-streaming errors. Kept as a
    /// dedicated constructor so `streaming.rs` can surface errors without
    /// going through `anyhow`.
    pub(super) fn from_reasoner_error(e: &aura_reasoner::ReasonerError) -> Self {
        match e {
            aura_reasoner::ReasonerError::InsufficientCredits(msg) => {
                Self::InsufficientCredits(msg.clone())
            }
            aura_reasoner::ReasonerError::RateLimited { message, .. } => {
                Self::RateLimited(message.clone())
            }
            // Defensive fallback: foreign providers (mock, test
            // harnesses, third-party `ModelProvider` impls) sometimes
            // report rate-limit conditions as `Internal(..)` /
            // `Api { .. }` / `Request(..)` rather than the dedicated
            // `RateLimited` variant. Phase 5 made the kernel gateway
            // preserve the typed `ReasonerError` end-to-end, but we
            // keep this string check so those out-of-band errors still
            // surface as `rate_limit` to SSE consumers.
            other if looks_like_rate_limited(&other.to_string()) => {
                Self::RateLimited(other.to_string())
            }
            other if other.is_context_overflow() => Self::PromptTooLong(other.to_string()),
            other => Self::Fatal(other.to_string()),
        }
    }
}

/// Detect a rate-limit error from free-form message text. See
/// [`LlmCallError::from_reasoner_error`] for the rationale — this is a
/// defensive fallback for provider implementations that lose the
/// typed `ReasonerError::RateLimited` variant.
fn looks_like_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("rate limited")
        || lower.contains("rate_limited")
        || lower.contains("too many requests")
}

fn classify_reasoner_error(e: &aura_reasoner::ReasonerError) -> LlmCallError {
    LlmCallError::from_reasoner_error(e)
}

impl AgentLoop {
    /// Call the model and translate errors.
    ///
    /// Uses streaming when `event_tx` is present, non-streaming otherwise.
    pub(super) async fn call_model(
        &self,
        provider: &dyn ModelProvider,
        request: ModelRequest,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
    ) -> Result<ModelResponse, LlmCallError> {
        let stream_timeout = self.config.stream_timeout;

        timeout(stream_timeout, async {
            if event_tx.is_some() {
                self.complete_with_streaming(provider, request, event_tx, cancellation_token)
                    .await
            } else {
                provider
                    .complete(request)
                    .await
                    .map_err(|e| classify_reasoner_error(&e))
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(LlmCallError::Fatal(format!(
                "Model call timed out after {stream_timeout:?}"
            )))
        })
    }
}

// ---------------------------------------------------------------------------
// Response accumulation
// ---------------------------------------------------------------------------

/// Accumulate token counts, text, and thinking from the model response.
pub(super) fn accumulate_response(state: &mut LoopState, response: &ModelResponse) {
    state.result.total_input_tokens += response.usage.input_tokens;
    state.result.total_output_tokens += response.usage.output_tokens;
    state.result.total_cache_creation_input_tokens += response
        .usage
        .cache_creation_input_tokens
        .unwrap_or_default();
    state.result.total_cache_read_input_tokens +=
        response.usage.cache_read_input_tokens.unwrap_or_default();

    // Record the *latest* iteration's cache hit/miss counts on the
    // per-turn breakdown. The wire `ContextBreakdown` surfaces this
    // so the UI can render a "Cached this turn" row. If the agent
    // loop runs multiple iterations, the last one wins -- that's the
    // freshest view of "what did the model just see from cache?".
    state.result.context_breakdown.cache_read_tokens =
        response.usage.cache_read_input_tokens.unwrap_or(0);
    state.result.context_breakdown.cache_creation_tokens =
        response.usage.cache_creation_input_tokens.unwrap_or(0);

    for block in &response.message.content {
        match block {
            ContentBlock::Text { text } => state.result.total_text.push_str(text),
            ContentBlock::Thinking { thinking, .. } => {
                state.result.total_thinking.push_str(thinking);
            }
            _ => {}
        }
    }

    state.messages.push(response.message.clone());
    summarize_write_inputs(&mut state.messages);

    #[allow(clippy::cast_possible_truncation)]
    let message_tokens =
        (compaction::estimate_message_chars(&state.messages) / CHARS_PER_TOKEN) as u64;
    let provider_tokens = response
        .usage
        .input_tokens
        .saturating_add(response.usage.output_tokens)
        .saturating_add(
            response
                .usage
                .cache_creation_input_tokens
                .unwrap_or_default(),
        )
        .saturating_add(response.usage.cache_read_input_tokens.unwrap_or_default());
    let estimated_context_tokens = provider_tokens.max(message_tokens);
    state.last_context_tokens_estimate = Some(estimated_context_tokens);
    state.result.estimated_context_tokens = estimated_context_tokens;
}

/// Replace large write-tool inputs with summaries to save context space.
fn summarize_write_inputs(messages: &mut [Message]) {
    let Some(last_msg) = messages.last_mut() else {
        return;
    };
    for block in &mut last_msg.content {
        if let ContentBlock::ToolUse { name, input, .. } = block {
            if let Some(summarized) = helpers::summarize_write_input(name, input) {
                *input = summarized;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MaxTokens stop-reason handling
// ---------------------------------------------------------------------------

/// Handle `StopReason::MaxTokens` — inject error results for pending tool calls.
///
/// Returns `true` if the loop should continue, `false` if it should break.
pub(super) fn handle_max_tokens(
    config: &AgentLoopConfig,
    response: &ModelResponse,
    state: &mut LoopState,
) -> bool {
    let pending_tools = extract_pending_tools(response);
    if pending_tools.is_empty() {
        return false;
    }

    warn!(
        pending = pending_tools.len(),
        "MaxTokens with pending tool_use blocks — injecting error results"
    );

    // Signal to `LoopState::begin_iteration` that the next iteration
    // must NOT taper `thinking_budget` — the model is about to retry
    // the dropped tool call(s) and needs the full budget to fit the
    // JSON that just got cut off. Without this reset, a task that
    // hits `max_tokens` mid-edit on iteration N+1 would retry on
    // iteration N+2 with an already-tapered budget and truncate
    // again, producing the observed loop of repeated
    // `MaxTokens with pending tool_use blocks` warnings.
    state.thinking.restore_next_iteration = true;

    let results: Vec<(String, ToolResultContent, bool)> = pending_tools
        .iter()
        .map(|pt| {
            let text = synthetic_truncation_message(pt);
            (pt.id.clone(), ToolResultContent::text(text), true)
        })
        .collect();

    state.messages.push(Message::tool_results(results));

    if config.max_context_tokens.is_some() {
        let tier = compaction::CompactionConfig::aggressive();
        compaction::compact_older_messages(&mut state.messages, &tier);
        sanitize::validate_and_repair(&mut state.messages);
    }

    true
}

/// Build the synthetic `tool_result` body injected when a tool call is
/// recovered from a `max_tokens`-truncated stream. Kept as a free
/// function so tests can pin the exact wording the model sees, and so
/// the per-tool branches stay readable.
fn synthetic_truncation_message(pt: &PendingTool) -> String {
    match pt.name.as_str() {
        "write_file" => match pt.path.as_deref() {
            Some(path) => format!(
                "Error: Response was truncated (max_tokens) mid-`write_file`. \
                 Target path: `{path}`. Partial content (if any) is NOT on disk. \
                 Next turn: call `edit_file` on `{path}` with `append_after_eof` to add \
                 remaining content incrementally, or call `write_file` with only the \
                 skeleton (module-doc + imports + one stub) and switch to `edit_file` \
                 appends for the rest."
            ),
            None => "Error: Response was truncated (max_tokens) mid-`write_file` \
                 (no target path recovered). Next turn: retry with the skeleton \
                 (module-doc + imports + one stub) and use `edit_file` \
                 `append_after_eof` for the rest."
                .to_string(),
        },
        "edit_file" => match pt.path.as_deref() {
            Some(path) => format!(
                "Error: Response was truncated (max_tokens) mid-`edit_file`. \
                 Target path: `{path}`. No changes were applied on disk. \
                 Next turn: split the edit into TWO smaller `edit_file` calls \
                 (e.g. change one function or block at a time) rather than one \
                 large diff. Your next `max_tokens` budget is restored to full \
                 for the retry, but each individual tool call should fit in a \
                 few hundred lines of diff."
            ),
            None => "Error: Response was truncated (max_tokens) mid-`edit_file` \
                 (no target path recovered). Next turn: retry with a smaller, \
                 targeted edit scoped to a single function or block."
                .to_string(),
        },
        other => format!(
            "Error: Response was truncated (max_tokens). Tool '{other}' was not executed. \
             Please try again with a simpler approach or break the task into smaller steps."
        ),
    }
}

/// Subset of a pending `tool_use` block used to shape the synthetic
/// error injected on `max_tokens`. `path` is best-effort — extracted
/// from the partial input when it serialized cleanly enough to decode
/// the `path` field before truncation hit.
struct PendingTool {
    id: String,
    name: String,
    path: Option<String>,
}

// ---------------------------------------------------------------------------
// Narration budget (Phase 4 live steering)
// ---------------------------------------------------------------------------

/// Build the steering message text injected at the soft budget. Shared
/// with tests so the assertion and the production string cannot drift.
pub(super) fn narration_steering_message(token_count: usize) -> String {
    format!(
        "[harness steering] The last turns produced {token_count} tokens of text with no tool \
         calls. On your next turn, call exactly ONE tool (read_file, search_code, or write_file \
         \u{2264} {WRITE_FILE_CHUNK_BYTES} bytes). Do NOT narrate a plan."
    )
}

/// Update the per-turn narration counter and, when budgets are crossed,
/// inject a steering user message or stamp a stop-reason override.
///
/// Returns `true` when the loop should break immediately (hard budget
/// exhausted). The caller is expected to invoke this after
/// [`accumulate_response`] and the stop-reason dispatch.
pub(super) fn update_narration_budget(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut super::LoopState,
    response: &ModelResponse,
) -> bool {
    let had_tool_call = response
        .message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

    if had_tool_call {
        state.counters.last_turn_had_tool_call = true;
        state.counters.consecutive_narration_tokens = 0;
        return false;
    }

    state.counters.last_turn_had_tool_call = false;
    let added = usize::try_from(response.usage.output_tokens).unwrap_or(usize::MAX);
    state.counters.consecutive_narration_tokens = state
        .counters
        .consecutive_narration_tokens
        .saturating_add(added);

    // Hard budget takes precedence: we do not want to inject a steering
    // message on a turn we are already aborting.
    if state.counters.consecutive_narration_tokens >= NARRATION_TOKEN_HARD_BUDGET {
        let msg = format!(
            "[harness steering] Narration budget exhausted after {} tokens without a tool call. \
             Stopping the turn so the orchestrator can decompose the task.",
            state.counters.consecutive_narration_tokens
        );
        warn!(
            tokens = state.counters.consecutive_narration_tokens,
            "narration hard budget exhausted, forcing stop_reason_override"
        );
        super::streaming::emit(
            event_tx,
            AgentLoopEvent::Error {
                code: "narration_budget_exhausted".to_string(),
                message: msg,
                recoverable: true,
            },
        );
        state.result.stop_reason_override = Some("narration_budget_exhausted".to_string());
        state.result.stalled = true;
        return true;
    }

    if state.counters.consecutive_narration_tokens >= NARRATION_TOKEN_SOFT_BUDGET {
        let steer = narration_steering_message(state.counters.consecutive_narration_tokens);
        info!(
            tokens = state.counters.consecutive_narration_tokens,
            "narration soft budget crossed, injecting steering user message"
        );
        state.messages.push(Message::user(steer.clone()));
        super::streaming::emit(event_tx, AgentLoopEvent::Warning(steer));
        state.counters.consecutive_narration_tokens = 0;
    }

    false
}

fn extract_pending_tools(response: &ModelResponse) -> Vec<PendingTool> {
    response
        .message
        .content
        .iter()
        .filter_map(|block| {
            if let ContentBlock::ToolUse { id, name, input } = block {
                let path = input
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                Some(PendingTool {
                    id: id.clone(),
                    name: name.clone(),
                    path,
                })
            } else {
                None
            }
        })
        .collect()
}
