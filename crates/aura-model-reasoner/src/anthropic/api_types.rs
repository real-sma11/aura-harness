use serde::{Deserialize, Serialize};

// ============================================================================
// API Types (matching Anthropic's JSON schema)
// ============================================================================

#[derive(Debug, Serialize)]
pub(super) struct ApiRequest {
    pub model: String,
    /// `None` when the caller did not supply a system prompt; the field
    /// is omitted from the wire payload entirely so Anthropic does not
    /// reject an empty system block (see `build_system_block`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ApiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ApiToolChoice>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ApiThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<ApiOutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub(super) struct ApiMessage {
    pub role: String,
    pub content: Vec<ApiContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ApiContent {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
    /// Thinking content block - required when extended thinking is enabled.
    /// Must be echoed back to the API in multi-turn conversations.
    Thinking {
        thinking: String,
        /// Signature is required when echoing thinking blocks back to the API
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Image {
        source: ApiImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ApiImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ApiTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<serde_json::Value>,
    /// Fine-grained tool streaming flag (Anthropic API).
    /// See <https://docs.anthropic.com/en/docs/agents-and-tools/tool-use/fine-grained-tool-streaming>.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eager_input_streaming: Option<bool>,
}

/// Anthropic `tool_choice` wire shape.
///
/// Phase 3: each variant carries an optional
/// `disable_parallel_tool_use: bool` so callers can opt out of
/// Anthropic's default parallel tool-use behaviour. The field is the
/// current documented shape for parallel-tool-use control on the
/// `messages` API; when `None`, serde skips it and the wire payload
/// matches the pre-Phase-3 `{"type": "auto"}` exactly (so existing
/// log-summary tests and on-the-wire behaviour stay byte-identical
/// for the default `parallel_tool_use: true` case).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ApiToolChoice {
    Auto {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Any {
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
    Tool {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        disable_parallel_tool_use: Option<bool>,
    },
}

#[derive(Debug, Deserialize)]
pub(super) struct ApiResponse {
    pub id: String,
    pub model: String,
    pub content: Vec<ApiContent>,
    pub stop_reason: Option<String>,
    pub usage: ApiUsage,
}

#[derive(Debug, Deserialize)]
#[allow(clippy::struct_field_names)]
pub(super) struct ApiUsage {
    #[serde(alias = "prompt_tokens")]
    pub input_tokens: u64,
    #[serde(alias = "completion_tokens")]
    pub output_tokens: u64,
    #[serde(default, alias = "prompt_cache_miss_tokens")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, alias = "prompt_cache_hit_tokens", alias = "cached_tokens")]
    pub cache_read_input_tokens: Option<u64>,
}

// ============================================================================
// Streaming API Types
// ============================================================================

/// Request with streaming enabled.
#[derive(Debug, Serialize)]
pub(super) struct StreamingApiRequest {
    pub model: String,
    /// See [`ApiRequest::system`] for why this is optional.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<serde_json::Value>,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ApiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ApiToolChoice>,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ApiThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_config: Option<ApiOutputConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<&'static str>,
}

/// Internal API representation of the extended thinking configuration.
#[derive(Debug, Serialize)]
pub(super) struct ApiThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
pub(super) struct ApiOutputConfig {
    pub effort: String,
}

// ============================================================================
// SSE Event Types
// ============================================================================

/// SSE event types from Anthropic.
///
/// These types are used for deserializing SSE events from the Anthropic API.
/// Some fields are parsed but not directly used (they're used for proper deserialization).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(super) enum SseEvent {
    MessageStart {
        message: SseMessageStart,
    },
    ContentBlockStart {
        index: u32,
        content_block: SseContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: SseDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: SseMessageDeltaContent,
        usage: Option<SseUsageDelta>,
    },
    MessageStop,
    Ping,
    Error {
        error: SseError,
    },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(super) struct SseMessageStart {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub usage: Option<SseUsageStart>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code, clippy::struct_field_names)]
pub(super) struct SseUsageStart {
    #[serde(alias = "prompt_tokens")]
    pub input_tokens: u64,
    #[serde(default, alias = "prompt_cache_miss_tokens")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, alias = "prompt_cache_hit_tokens", alias = "cached_tokens")]
    pub cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(super) enum SseContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(super) enum SseDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "signature_delta")]
    Signature { signature: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
}

#[derive(Debug, Deserialize)]
pub(super) struct SseMessageDeltaContent {
    pub stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SseUsageDelta {
    #[serde(alias = "completion_tokens")]
    pub output_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct SseError {
    pub message: String,
    /// Anthropic-shape error type (e.g. `overloaded_error`, `api_error`,
    /// `rate_limit_error`). Optional because some proxies / intermediate
    /// layers emit a bare `{"error":{"message":"..."}}` shape.
    #[serde(rename = "type", default)]
    pub error_type: Option<String>,
    /// Optional request id embedded in the SSE error body by some
    /// proxies (Anthropic itself does *not* do this, but
    /// `aura-router` and similar layers sometimes surface the
    /// originating request id here). Used only as a fallback when the
    /// response-header `x-request-id` was unavailable.
    #[serde(default)]
    pub request_id: Option<String>,
}
