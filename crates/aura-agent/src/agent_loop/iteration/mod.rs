//! Per-iteration logic: LLM calls, response accumulation, and stop-reason handling.

use aura_model_reasoner::{
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
use aura_config::CHARS_PER_TOKEN;
use aura_context_compaction as compaction;

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
    /// Convert a structured [`aura_model_reasoner::ReasonerError`] into an
    /// [`LlmCallError`] with the same credit/context/fatal classification
    /// the loop already applies to non-streaming errors. Kept as a
    /// dedicated constructor so `streaming.rs` can surface errors without
    /// going through `anyhow`.
    pub(super) fn from_reasoner_error(e: &aura_model_reasoner::ReasonerError) -> Self {
        match e {
            aura_model_reasoner::ReasonerError::InsufficientCredits(msg) => {
                Self::InsufficientCredits(msg.clone())
            }
            aura_model_reasoner::ReasonerError::RateLimited { message, .. } => {
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

fn classify_reasoner_error(e: &aura_model_reasoner::ReasonerError) -> LlmCallError {
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
/// Phase 4.6 removed the post-response compaction call that used to
/// live at the tail of this function. Compaction now runs exactly
/// once per sampling turn from [`super::context::compact_if_needed`]
/// at the top of the next iteration. The previous double-call was
/// redundant: every code path that consumed
/// [`compaction::SummaryInput`] from this function also re-entered
/// `compact_if_needed` the very next iteration, so dropping the
/// post-response branch eliminates one full `Compactor::compact_messages`
/// pass per iteration without changing the observable transcript.
///
/// `iteration` is the 0-based index of the model call whose response
/// is being folded in. Kept on the signature for symmetry with
/// [`super::context::effective_compaction_request_kind`] (the next
/// `compact_if_needed` call uses it) and so callers don't have to
/// thread the counter through a different shape per phase.
///
/// Returns `true` when leaked tool-call markup was scrubbed from the
/// assistant text (see [`super::text_sanitize`]). The turn loop uses
/// this to recover the "model wrote a tool call as text, so no tool
/// ran, and the turn ended after one message" failure: the scrubbed
/// leftover text still counts as visible output, so the never-silent
/// no-op nudge does not fire — the caller needs the explicit signal to
/// re-prompt instead of ending the turn.
pub(super) fn accumulate_response(
    _config: &AgentLoopConfig,
    state: &mut LoopState,
    response: &ModelResponse,
    _iteration: usize,
) -> bool {
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

    // Defensively strip any tool-call markup the model leaked into a
    // text block (Anthropic `<invoke>` / `<function_calls>` XML or the
    // hybrid `[tool_use ... name="...">` shape — see
    // `super::text_sanitize`). Done before the message enters history so
    // the markup is neither re-fed to the model on the next iteration
    // (reinforcing the behaviour) nor carried into the accumulated
    // result text.
    let (sanitized_message, scrubbed_markup) =
        super::text_sanitize::sanitize_message(&response.message);
    if scrubbed_markup {
        warn!("Scrubbed leaked tool-call markup from assistant text before adding to history");
    }

    for block in &sanitized_message.content {
        match block {
            ContentBlock::Text { text } => state.result.total_text.push_str(text),
            ContentBlock::Thinking { thinking, .. } => {
                state.result.total_thinking.push_str(thinking);
            }
            _ => {}
        }
    }

    state.messages.push(sanitized_message);

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

    scrubbed_markup
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
    let pending_tools = super::tool_pipeline::tool_calls(response);
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
        .map(|tc| {
            let text = synthetic_truncation_message(tc);
            (tc.id.clone(), ToolResultContent::text(text), true)
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
/// [`aura_context_prompts::model_messages::max_tokens`]; this helper is just
/// the per-tool dispatcher.
///
/// `path` is best-effort — extracted from the (possibly partial)
/// `tool_use` input JSON. When the truncated stream serialised the
/// `path` field cleanly enough to survive, the wording is sharper
/// (names the file in the synthetic error); otherwise we fall back
/// to the path-less template.
fn synthetic_truncation_message(tc: &crate::types::ToolCallInfo) -> String {
    use aura_context_prompts::model_messages::max_tokens;
    let path = tc.input.get("path").and_then(|v| v.as_str());
    match tc.name.as_str() {
        "write_file" => match path {
            Some(p) => max_tokens::write_file_truncation_with_path(p),
            None => max_tokens::WRITE_FILE_TRUNCATION_NO_PATH.to_string(),
        },
        "edit_file" => match path {
            Some(p) => max_tokens::edit_file_truncation_with_path(p),
            None => max_tokens::EDIT_FILE_TRUNCATION_NO_PATH.to_string(),
        },
        other => max_tokens::generic_tool_truncation(other),
    }
}
