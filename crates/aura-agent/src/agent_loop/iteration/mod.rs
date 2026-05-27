//! Per-iteration logic: LLM calls, response accumulation, and stop-reason handling.

use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelResponse, ToolResultContent,
};
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::dup_audit;
use crate::events::AgentLoopEvent;
use crate::sanitize;
use crate::types::AgentLoopResult;
use aura_compaction as compaction;
use aura_config::CHARS_PER_TOKEN;

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
///
/// `iteration` is the 0-based index of the model call whose response
/// is being folded in. It is forwarded to
/// [`super::context::effective_compaction_request_kind`] so the post-call
/// compaction-policy reads the same request kind the next wire request
/// will carry, instead of the stale [`AgentLoopConfig::request_kind`]
/// (which never advances past `DevLoopBootstrap` for dev-loop runs and
/// otherwise pins cap-pressure at 1.0 for the whole task — see the
/// helper's doc comment for the full failure mode).
pub(super) fn accumulate_response(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    response: &ModelResponse,
    iteration: usize,
) -> Option<compaction::SummaryInput> {
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

    let raw_message_bytes = compaction::estimate_message_chars(&state.messages);
    #[allow(clippy::cast_possible_truncation)]
    let message_tokens = (raw_message_bytes / CHARS_PER_TOKEN) as u64;
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

    if super::context::compaction_disabled_by_env() {
        return None;
    }

    let reserved_output_tokens = config
        .max_context_tokens
        .map_or(u64::from(config.max_tokens), |max_ctx| {
            u64::from(config.max_tokens).min(max_ctx)
        });
    let request_kind = super::context::effective_compaction_request_kind(config, iteration);
    let report = compaction::Compactor::new().compact_messages(compaction::CompactionInput {
        messages: &mut state.messages,
        policy: compaction::CompactionPolicy {
            current_context_tokens: Some(message_tokens),
            raw_message_bytes: Some(raw_message_bytes),
            request_kind: Some(request_kind),
            ..compaction::CompactionPolicy::new(
                config.max_context_tokens,
                estimated_context_tokens,
                reserved_output_tokens,
            )
        },
    });
    if report.reduced() {
        let compacted_chars = compaction::estimate_message_chars(&state.messages);
        #[allow(clippy::cast_possible_truncation)]
        let compacted_tokens = (compacted_chars / CHARS_PER_TOKEN) as u64;
        let updated_estimate = provider_tokens.max(compacted_tokens);
        state.last_context_tokens_estimate = Some(updated_estimate);
        state.result.estimated_context_tokens = updated_estimate;
    }

    match report.action {
        compaction::CompactionAction::NeedsSummary(input) => Some(input),
        compaction::CompactionAction::Applied(_) | compaction::CompactionAction::None => None,
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

    dup_audit::audit_tool_result_duplicates(&state.messages, "handle_max_tokens.pre");
    state.messages.push(Message::tool_results(results));
    dup_audit::audit_tool_result_duplicates(&state.messages, "handle_max_tokens.post");

    if config.max_context_tokens.is_some() && !super::context::compaction_disabled_by_env() {
        let tier = compaction::CompactionConfig::aggressive();
        compaction::compact_older_messages(&mut state.messages, &tier);
        sanitize::validate_and_repair(&mut state.messages);
    }

    true
}

/// Build the synthetic `tool_result` body injected when a tool call is
/// recovered from a `max_tokens`-truncated stream. Wording lives in
/// [`aura_prompts::model_messages::max_tokens`]; this helper is just
/// the per-tool dispatcher.
fn synthetic_truncation_message(pt: &PendingTool) -> String {
    use aura_prompts::model_messages::max_tokens;
    match pt.name.as_str() {
        "write_file" => match pt.path.as_deref() {
            Some(path) => max_tokens::write_file_truncation_with_path(path),
            None => max_tokens::WRITE_FILE_TRUNCATION_NO_PATH.to_string(),
        },
        "edit_file" => match pt.path.as_deref() {
            Some(path) => max_tokens::edit_file_truncation_with_path(path),
            None => max_tokens::EDIT_FILE_TRUNCATION_NO_PATH.to_string(),
        },
        other => max_tokens::generic_tool_truncation(other),
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
