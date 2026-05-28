//! Inbound (client → server) wire messages and their payloads.
//!
//! [`InboundMessage`] is the top-level enum sent from a websocket
//! client into the harness. Each variant carries one of the payload
//! structs defined in this module.
//!
//! Phase A note: the `SessionInit` first-frame contract was deleted
//! when `POST /v1/run` + `WS /stream/:run_id` replaced the legacy
//! `WS /stream` handshake. The WS now opens against a run id that
//! already exists, so `InboundMessage` no longer carries a
//! session-init variant — all session configuration ships on
//! [`crate::RuntimeRequest`] over HTTP instead.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[cfg(feature = "typescript")]
use ts_rs::TS;

use crate::common::{ToolApprovalDecision, ToolApprovalRemember};

/// Top-level inbound message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub enum InboundMessage {
    /// Send a user message for processing.
    UserMessage(UserMessage),
    /// Cancel the current turn.
    Cancel,
    /// Respond to an approval request.
    ApprovalResponse(ApprovalResponse),
    /// Respond to a live tool approval prompt.
    ToolApprovalResponse(ToolApprovalResponse),
    /// Request image or 3D generation.
    GenerationRequest(GenerationRequest),
}

/// A prior conversation message used to hydrate session history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

/// Keyword-driven classifier spec shipped on
/// [`crate::AgentCapabilities`].
///
/// Matches the JSON shape that
/// `aura-tools::IntentClassifier::from_profile_json` deserializes,
/// extended with `tool_domains` so the harness can answer "which
/// domain does this tool belong to?" without hard-coding the
/// mapping in its binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct IntentClassifierSpec {
    /// Domain names that are always visible (tier-1). Snake-case
    /// strings like `"project"`, `"agent"`, `"execution"`,
    /// `"monitoring"`.
    pub tier1_domains: Vec<String>,
    /// Keyword rules that expand the visible domain set tier-2 on
    /// demand.
    pub classifier_rules: Vec<IntentClassifierRule>,
    /// Mapping from tool name → domain. Any tool whose domain is
    /// in the resolved visible set is kept on a turn.
    #[serde(default)]
    pub tool_domains: HashMap<String, String>,
}

/// One keyword → domain rule for [`IntentClassifierSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct IntentClassifierRule {
    pub domain: String,
    pub keywords: Vec<String>,
}

/// Per-session model overrides applied on top of the harness's
/// env-default router config.
///
/// All LLM traffic flows through aura-router (the AURA proxy) using
/// a per-request JWT; there is no direct-provider path, so this
/// struct only carries knobs that still mean something for proxy
/// routing: model name, fallback model, prompt-caching toggle.
/// `None` on a field means "leave the harness default unchanged".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct SessionModelOverrides {
    /// Optional default model for this session.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Optional fallback model used on 429/529 retries.
    #[serde(default)]
    pub fallback_model: Option<String>,
    /// Optional override for whether Anthropic prompt-caching
    /// directives should be attached.
    #[serde(default)]
    pub prompt_caching_enabled: Option<bool>,
    /// Optional stable cache key forwarded to aura-router for
    /// OpenAI-family prompt caching (`prompt_cache_key` in the
    /// OpenAI API).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// Optional retention hint paired with [`Self::prompt_cache_key`].
    /// Wire values are `"in_memory"` (default, ~5–10 min) or
    /// `"24h"` (extended retention on newer OpenAI models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<String>,
}

/// Payload for `user_message`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct UserMessage {
    pub content: String,
    /// Optional list of tool names the user wants prioritized for
    /// this message. When set, the agent loop will filter tools and
    /// set `tool_choice` on the first iteration to explicitly
    /// direct the model toward these tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_hints: Option<Vec<String>>,
    /// Optional image/text attachments (base64-encoded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<MessageAttachment>>,
}

/// A user-supplied attachment (image or text file) sent with a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct MessageAttachment {
    /// `"image"` or `"text"`.
    #[serde(rename = "type")]
    pub type_: String,
    /// MIME type (e.g. `"image/png"`).
    pub media_type: String,
    /// Base64-encoded payload.
    pub data: String,
    /// Optional filename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// URL to fetch content from (e.g. S3). When set, `data` may be
    /// empty and the consumer should fetch the content from this
    /// URL instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

/// Payload for `approval_response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ApprovalResponse {
    pub tool_use_id: String,
    pub approved: bool,
}

/// Payload for `tool_approval_response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct ToolApprovalResponse {
    pub request_id: String,
    pub decision: ToolApprovalDecision,
    pub remember: ToolApprovalRemember,
}

/// Payload for `generation_request`.
///
/// Fields are mode-dependent:
/// - `mode == "image"`: uses `prompt` (required), `model`, `size`, `images`, `is_iteration`
/// - `mode == "3d"`:    uses `image_url` (required), `prompt` (optional hint)
///
/// Both modes accept `project_id` for artifact storage. 3D
/// generation also accepts `parent_id` to link a generated model to
/// a source image artifact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct GenerationRequest {
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_iteration: Option<bool>,
    /// Video generation: aspect ratio (e.g. "16:9", "9:16").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,
    /// Video generation: duration in seconds (4, 6, or 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u8>,
    /// Video generation: resolution (e.g. "720p", "1080p", "4k").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Video generation: whether to generate audio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generate_audio: Option<bool>,
}
