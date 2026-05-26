//! # aura-reasoner
//!
//! Provider-agnostic model interface for Aura.
//!
//! This crate provides:
//! - Normalized conversation types (`Message`, `ContentBlock`, `ToolDefinition`)
//! - `ModelProvider` trait for provider-agnostic completions
//! - `AnthropicProvider` implementation using `reqwest` for HTTP communication with the Anthropic API
//! - `MockProvider` for testing
//!
//! ## Architecture
//!
//! The reasoner abstraction separates AURA's deterministic kernel from
//! probabilistic model calls. All model interactions go through the
//! `ModelProvider` trait, enabling:
//!
//! - Provider switching (Anthropic, `OpenAI`, local models)
//! - Recording/replay of model outputs for determinism
//! - Testing with mock providers

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod anthropic;
mod error;
mod kernel_propose;
mod mock;
pub mod provider_factory;
pub mod types;

pub use provider_factory::{
    default_provider as default_provider_from_env, mock_provider, with_session_overrides,
    ProviderSelection, SessionOverrides,
};

pub(crate) fn truncate_body(body: &str, max_len: usize) -> String {
    if body.len() <= max_len {
        body.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &body[..end])
    }
}

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use error::ReasonerError;

// ============================================================================
// Retry observability (debug.retry emission)
// ============================================================================

/// Metadata for a single upcoming retry attempt, surfaced to external
/// observers via [`DEBUG_RETRY_OBSERVER`]. The provider invokes the
/// observer *before* it sleeps, so subscribers can time the backoff
/// themselves if they need more than `wait_ms` worth of accuracy.
#[derive(Debug, Clone)]
pub struct RetryInfo {
    /// Short class name, e.g. `"rate_limited_429"`, `"cloudflare_block"`, `"upstream_5xx"`.
    pub reason: String,
    /// 1-based attempt number that will now occur (first retry = 2).
    pub attempt: u32,
    /// The delay the provider is about to sleep, in milliseconds.
    pub wait_ms: u64,
    /// Provider name (e.g. `"anthropic"`).
    pub provider: String,
    /// Model name the retry is happening against.
    pub model: String,
}

/// Trait-object callback type used by [`DEBUG_RETRY_OBSERVER`]. Agents
/// wrap an [`std::sync::Arc`]-ed closure that forwards `RetryInfo` into
/// their event stream.
pub type RetryObserver = std::sync::Arc<dyn Fn(RetryInfo) + Send + Sync>;

tokio::task_local! {
    /// Task-local observer invoked on every retry decision inside the
    /// provider's retry loop. Left unset in normal call paths; the
    /// `AgentLoop` sets it with [`tokio::task::LocalKey::scope`] around
    /// `ModelProvider` calls to route `debug.retry` emissions back into
    /// its event channel without widening the `ModelProvider` trait.
    pub static DEBUG_RETRY_OBSERVER: RetryObserver;
}

/// Invoke the task-local [`DEBUG_RETRY_OBSERVER`] if one has been set
/// for the current task. No-op when nothing is subscribed. Intended for
/// provider-internal use.
pub fn emit_retry(info: RetryInfo) {
    let _ = DEBUG_RETRY_OBSERVER.try_with(|obs| obs(info));
}
pub use kernel_propose::{ProposeLimits, ProposeRequest, RecordSummary};
pub use mock::{MockProvider, MockResponse};
pub use types::{
    AccumulatedToolUse, CacheControl, ContentBlock, ImageSource, MaxTokens, Message,
    ModelContentProfile, ModelContractVerdict, ModelContractViolationReason, ModelName,
    ModelRequest, ModelRequestContractViolation, ModelRequestKind, ModelRequestMetadata,
    ModelResponse, PartialToolUse, PromptCacheRetention, ProviderTrace, Role, StopReason,
    StreamAccumulator, StreamContentType, StreamEvent, Temperature, ThinkingConfig,
    ThinkingEffort, ToolChoice, ToolDefinition, ToolResultContent, Usage,
};

use futures_util::Stream;
use std::pin::Pin;

use async_trait::async_trait;
use tracing::debug;

// ============================================================================
// ModelProvider Trait (New in Spec-02)
// ============================================================================

/// Type alias for a boxed stream of streaming events.
pub type StreamEventStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, ReasonerError>> + Send + 'static>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResponseOutputShape {
    pub content_block_count: usize,
    pub text_bytes: usize,
    pub thinking_bytes: usize,
    pub tool_use_count: usize,
}

pub(crate) fn response_output_shape(response: &ModelResponse) -> ResponseOutputShape {
    let mut shape = ResponseOutputShape {
        content_block_count: response.message.content.len(),
        ..ResponseOutputShape::default()
    };

    for block in &response.message.content {
        match block {
            ContentBlock::Text { text } => shape.text_bytes += text.len(),
            ContentBlock::Thinking { thinking, .. } => shape.thinking_bytes += thinking.len(),
            ContentBlock::ToolUse { .. } => shape.tool_use_count += 1,
            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {}
        }
    }

    shape
}

// The body clones each field separately; passing by value matches the
// call sites where `response` is their last use of the value. TODO(W5):
// refactor callers to pass `&ModelResponse` once the streaming
// adapter is split out per the Wave 6 plan.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn stream_from_response(response: ModelResponse) -> StreamEventStream {
    let content_block_count = response.message.content.len();
    let mut text_delta_count = 0usize;
    let mut thinking_delta_count = 0usize;
    let mut tool_use_count = 0usize;
    let mut text_bytes = 0usize;
    let mut thinking_bytes = 0usize;

    // Prepend the synthetic `HttpMeta` frame so consumers that
    // uniformly seed their accumulator from this event (streaming +
    // non-streaming fallback + mock paths) always see a single,
    // well-defined preamble. The value comes from the non-streaming
    // `complete()` path's header capture — `None` for mock / test
    // producers.
    let mut events: Vec<Result<StreamEvent, ReasonerError>> = vec![
        Ok(StreamEvent::HttpMeta {
            request_id: response.trace.provider_request_id.clone(),
        }),
        Ok(StreamEvent::MessageStart {
            message_id: response.trace.message_id.clone().unwrap_or_default(),
            model: response.trace.model.clone(),
            input_tokens: Some(response.usage.input_tokens),
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
            cache_read_input_tokens: response.usage.cache_read_input_tokens,
        }),
    ];

    for (index, block) in response.message.content.iter().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let idx = index as u32;
        match block {
            ContentBlock::Text { text } => {
                text_delta_count += 1;
                text_bytes += text.len();
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: idx,
                    content_type: StreamContentType::Text,
                }));
                events.push(Ok(StreamEvent::TextDelta { text: text.clone() }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                thinking_delta_count += 1;
                thinking_bytes += thinking.len();
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: idx,
                    content_type: StreamContentType::Thinking,
                }));
                events.push(Ok(StreamEvent::ThinkingDelta {
                    thinking: thinking.clone(),
                }));
                if let Some(sig) = signature {
                    events.push(Ok(StreamEvent::SignatureDelta {
                        signature: sig.clone(),
                    }));
                }
                events.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_use_count += 1;
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: idx,
                    content_type: StreamContentType::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                    },
                }));
                events.push(Ok(StreamEvent::InputJsonDelta {
                    partial_json: input.to_string(),
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: idx }));
            }
            _ => {}
        }
    }

    events.push(Ok(StreamEvent::MessageDelta {
        stop_reason: Some(response.stop_reason),
        output_tokens: response.usage.output_tokens,
    }));
    events.push(Ok(StreamEvent::MessageStop));

    debug!(
        model = %response.trace.model,
        content_block_count,
        text_delta_count,
        thinking_delta_count,
        tool_use_count,
        text_bytes,
        thinking_bytes,
        "Synthesized stream events from buffered model response"
    );

    Box::pin(futures_util::stream::iter(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_output_shape_counts_blocks_without_content() {
        let response = ModelResponse::new(
            StopReason::ToolUse,
            Message::new(
                Role::Assistant,
                vec![
                    ContentBlock::Text {
                        text: "hello".into(),
                    },
                    ContentBlock::Thinking {
                        thinking: "hidden".into(),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({ "path": "x" }),
                    },
                ],
            ),
            Usage::new(1, 2),
            ProviderTrace::new("test-model", 10),
        );

        assert_eq!(
            response_output_shape(&response),
            ResponseOutputShape {
                content_block_count: 3,
                text_bytes: 5,
                thinking_bytes: 6,
                tool_use_count: 1,
            }
        );
    }
}

/// Provider-agnostic interface for model completions.
///
/// This trait abstracts over different LLM providers (Anthropic, `OpenAI`, etc.)
/// allowing the kernel to work with any provider that implements this interface.
///
/// # Recording and Replay
///
/// During normal operation, the kernel calls `complete()` and records the
/// `ModelResponse`. During replay, the kernel loads the recorded response
/// instead of calling `complete()`, ensuring deterministic state reconstruction.
///
/// # Tool Use
///
/// When the model wants to use tools, it returns with `StopReason::ToolUse`.
/// The kernel extracts tool calls from the response message, executes them,
/// and continues the conversation with tool results.
///
/// # Streaming
///
/// For real-time output, use `complete_streaming()` which returns a stream
/// of `StreamEvent`s. This allows displaying text as it's generated.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Provider name (e.g., "anthropic", "openai", "mock").
    fn name(&self) -> &'static str;

    /// Complete a conversation, potentially with tool use.
    ///
    /// # Arguments
    ///
    /// * `request` - The model request containing system prompt, messages, and tools
    ///
    /// # Returns
    ///
    /// * `Ok(ModelResponse)` - The model's response with stop reason and content
    /// * `Err(_)` - If the request fails (network, auth, rate limit, etc.)
    ///
    /// # Errors
    ///
    /// Returns error if the provider request fails.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError>;

    /// Complete a conversation with streaming output.
    ///
    /// Returns a stream of `StreamEvent`s that can be processed in real-time.
    /// Use `StreamAccumulator` to collect events into a final `ModelResponse`.
    ///
    /// # Arguments
    ///
    /// * `request` - The model request containing system prompt, messages, and tools
    ///
    /// # Returns
    ///
    /// A stream of events. The stream ends with either `MessageStop` or `Error`.
    ///
    /// # Default Implementation
    ///
    /// Falls back to non-streaming `complete()` if not overridden.
    ///
    /// # Errors
    ///
    /// Returns error if the provider request fails.
    async fn complete_streaming(
        &self,
        request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let response = self.complete(request).await?;
        Ok(stream_from_response(response))
    }

    /// Check if the provider is available.
    ///
    /// This can be used for health checks and load balancing.
    async fn health_check(&self) -> bool;
}
