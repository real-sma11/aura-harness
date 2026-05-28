use super::message::Message;
use serde::{Deserialize, Serialize};

// ============================================================================
// Stop Reason
// ============================================================================

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model completed its turn naturally
    #[default]
    EndTurn,
    /// Model wants to use tools
    ToolUse,
    /// Hit the `max_tokens` limit
    MaxTokens,
    /// Hit a stop sequence
    StopSequence,
}

// ============================================================================
// Usage
// ============================================================================

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Number of input tokens
    pub input_tokens: u64,
    /// Number of output tokens
    pub output_tokens: u64,
    /// Cache creation input tokens (prompt caching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// Cache read input tokens (prompt caching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

impl Usage {
    /// Create new usage information.
    #[must_use]
    pub const fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    }

    /// Total tokens used.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// Set cache token counts.
    #[must_use]
    pub const fn with_cache(mut self, creation: Option<u64>, read: Option<u64>) -> Self {
        self.cache_creation_input_tokens = creation;
        self.cache_read_input_tokens = read;
        self
    }
}

// ============================================================================
// Provider Trace
// ============================================================================

/// Provider trace for debugging/logging.
///
/// The trace distinguishes two different identifiers that historically
/// lived in a single `request_id` field:
///
/// - [`message_id`](Self::message_id): the provider's internal message
///   id (e.g. Anthropic `message_start.message.id` / `msg_01…`). Survives
///   model fallbacks but is *not* what provider / router logs key on.
/// - [`provider_request_id`](Self::provider_request_id): the HTTP-level
///   `x-request-id` header returned by the upstream. This is the
///   correct key for correlating a single failed stream with provider
///   / `aura-router` logs.
///
/// Persisted run bundles (`llm_calls.jsonl`) that were written before
/// the split stored this field as `request_id`; the serde alias on
/// [`message_id`](Self::message_id) keeps those bundles loadable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderTrace {
    /// Provider-internal message id (Anthropic `message_start.message.id`).
    ///
    /// Aliased from the legacy `request_id` key to keep old persisted
    /// JSON readable — the previous serialization stored the Anthropic
    /// *message* id under `request_id`, which was confusing.
    #[serde(default, alias = "request_id", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// HTTP `x-request-id` captured from the streaming / non-streaming
    /// response headers. Distinct from [`message_id`](Self::message_id)
    /// and the right key for correlating with provider / router logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_request_id: Option<String>,
    /// Latency in milliseconds
    pub latency_ms: u64,
    /// Model that was used
    pub model: String,
}

impl ProviderTrace {
    /// Create a new provider trace.
    #[must_use]
    pub fn new(model: impl Into<String>, latency_ms: u64) -> Self {
        Self {
            message_id: None,
            provider_request_id: None,
            latency_ms,
            model: model.into(),
        }
    }

    /// Set the provider-internal message id.
    #[must_use]
    pub fn with_message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = Some(id.into());
        self
    }

    /// Set the HTTP-level `x-request-id` captured from the upstream.
    #[must_use]
    pub fn with_provider_request_id(mut self, id: impl Into<String>) -> Self {
        self.provider_request_id = Some(id.into());
        self
    }

    /// Legacy setter: stores the value under [`message_id`](Self::message_id)
    /// because older call sites passed the provider's message id.
    /// Prefer [`with_message_id`](Self::with_message_id) or
    /// [`with_provider_request_id`](Self::with_provider_request_id).
    #[must_use]
    #[deprecated(note = "Ambiguous: old call sites stored a message id here. Prefer \
                with_message_id or with_provider_request_id.")]
    pub fn with_request_id(self, id: impl Into<String>) -> Self {
        self.with_message_id(id)
    }

    /// Best-effort legacy accessor.
    ///
    /// Returns the HTTP request id if known, falling back to the
    /// provider message id. New code should read
    /// [`provider_request_id`](Self::provider_request_id) or
    /// [`message_id`](Self::message_id) directly.
    #[must_use]
    pub fn request_id(&self) -> Option<String> {
        self.provider_request_id
            .clone()
            .or_else(|| self.message_id.clone())
    }
}

// ============================================================================
// Model Response
// ============================================================================

/// Response from the model.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// Why the model stopped
    pub stop_reason: StopReason,
    /// The assistant message
    pub message: Message,
    /// Token usage
    pub usage: Usage,
    /// Provider trace information
    pub trace: ProviderTrace,
    /// Which model actually served the request (relevant after fallback).
    pub model_used: String,
}

impl ModelResponse {
    /// Create a new model response.
    #[must_use]
    pub fn new(
        stop_reason: StopReason,
        message: Message,
        usage: Usage,
        trace: ProviderTrace,
    ) -> Self {
        let model_used = trace.model.clone();
        Self {
            stop_reason,
            message,
            usage,
            trace,
            model_used,
        }
    }

    /// Check if the model wants to use tools.
    #[must_use]
    pub fn wants_tool_use(&self) -> bool {
        self.stop_reason == StopReason::ToolUse
    }

    /// Check if the turn is complete.
    #[must_use]
    pub fn is_end_turn(&self) -> bool {
        self.stop_reason == StopReason::EndTurn
    }
}
