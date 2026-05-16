use super::content_profile::{ModelRequestKind, ModelRequestMetadata};
use super::message::Message;
use super::tool::{ToolChoice, ToolDefinition};
use crate::error::ReasonerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

// ============================================================================
// Thinking Configuration
// ============================================================================

/// Per-request extended thinking configuration.
///
/// When set on a `ModelRequest`, the provider will enable extended thinking
/// with the specified budget. When `None`, the provider may apply its own
/// heuristic (e.g., auto-enable for capable models).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Token budget allocated for the thinking phase.
    /// Must be >= 1024 and < `max_tokens`.
    pub budget_tokens: u32,
}

// ============================================================================
// Strong-typed request primitives
// ============================================================================

/// Model identifier — never empty.
///
/// Wraps `Arc<str>` so cloning is cheap (the agent loop clones the model name
/// into every request). Construct via `ModelName::try_new` (validating),
/// `ModelName::from("…")` (panics on empty — intended for call sites that have
/// already validated the input), or the explicit `From<String>` impl.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ModelName(Arc<str>);

impl Serialize for ModelName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Self(Arc::from(s)))
    }
}

impl ModelName {
    /// Construct a new `ModelName`, rejecting empty / whitespace-only input.
    ///
    /// # Errors
    /// Returns [`ReasonerError::Internal`] if the model name is empty.
    pub fn try_new(name: impl Into<String>) -> Result<Self, ReasonerError> {
        let s = name.into();
        if s.trim().is_empty() {
            return Err(ReasonerError::Internal(
                "model name must not be empty".into(),
            ));
        }
        Ok(Self(Arc::from(s)))
    }

    /// Borrow the underlying string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ModelName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ModelName {
    /// Infallible conversion used by the builder. The builder re-validates on
    /// `build()`; callers that construct a `ModelName` directly from a `&str`
    /// must ensure the input is non-empty (kernel/agent code always does).
    fn from(s: &str) -> Self {
        Self(Arc::from(s))
    }
}

impl From<String> for ModelName {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

impl PartialEq<str> for ModelName {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ModelName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Maximum output tokens — always > 0.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MaxTokens(NonZeroU32);

impl MaxTokens {
    /// Construct a validated `MaxTokens` value.
    ///
    /// # Errors
    /// Returns [`ReasonerError::Internal`] when `value == 0`.
    pub fn try_new(value: u32) -> Result<Self, ReasonerError> {
        NonZeroU32::new(value)
            .map(Self)
            .ok_or_else(|| ReasonerError::Internal("max_tokens must be > 0".into()))
    }

    /// Return the raw `u32` for downstream API serialization.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

impl From<MaxTokens> for u32 {
    fn from(v: MaxTokens) -> Self {
        v.get()
    }
}

/// Sampling temperature, constrained to the range supported by the major
/// model providers (`0.0..=2.0`). Default: `1.0`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct Temperature(f32);

impl Temperature {
    /// Construct a validated temperature.
    ///
    /// # Errors
    /// Returns [`ReasonerError::Internal`] when `value` is outside the
    /// allowed range (`0.0..=2.0`) or non-finite.
    pub fn try_new(value: f32) -> Result<Self, ReasonerError> {
        if value.is_finite() && (0.0..=2.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ReasonerError::Internal(format!(
                "temperature {value} is outside the allowed range 0.0..=2.0"
            )))
        }
    }
}

impl Default for Temperature {
    fn default() -> Self {
        Self(1.0)
    }
}

impl From<Temperature> for f32 {
    fn from(v: Temperature) -> Self {
        v.0
    }
}

impl TryFrom<f32> for Temperature {
    type Error = ReasonerError;

    fn try_from(value: f32) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

/// Wire value for OpenAI's `prompt_cache_retention` field. `InMemory`
/// maps to OpenAI's default 5-10 minute cache; `Hours24` requests
/// the extended 24-hour retention available on newer OpenAI models.
/// The router translates this into the OpenAI-native field when
/// forwarding; aura-harness only carries the hint on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheRetention {
    InMemory,
    Hours24,
}

impl PromptCacheRetention {
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::InMemory => "in_memory",
            Self::Hours24 => "24h",
        }
    }
}

// ============================================================================
// Model Request
// ============================================================================

/// Request to the model.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    /// Model identifier (e.g., "claude-opus-4-6")
    pub model: ModelName,
    /// System prompt
    pub system: String,
    /// Conversation messages
    pub messages: Vec<Message>,
    /// Available tools
    pub tools: Vec<ToolDefinition>,
    /// Tool choice mode
    pub tool_choice: ToolChoice,
    /// Maximum tokens to generate
    pub max_tokens: MaxTokens,
    /// Sampling temperature
    pub temperature: Option<Temperature>,
    /// Extended thinking configuration. When `Some`, the provider enables
    /// thinking with the given budget. When `None`, provider-default behavior
    /// applies.
    pub thinking: Option<ThinkingConfig>,
    /// Optional JWT auth token for proxy routing.
    pub auth_token: Option<String>,
    /// Optional upstream provider family hint for managed proxy routing.
    pub upstream_provider_family: Option<String>,
    /// Project ID for X-Aura-Project-Id billing header.
    pub aura_project_id: Option<String>,
    /// Project-agent UUID for X-Aura-Agent-Id billing header.
    pub aura_agent_id: Option<String>,
    /// Storage session UUID for X-Aura-Session-Id billing header.
    pub aura_session_id: Option<String>,
    /// Org UUID for X-Aura-Org-Id billing header.
    pub aura_org_id: Option<String>,
    /// Optional stable cache key forwarded to the router so OpenAI-family
    /// upstreams can pin identical prefixes to the same backend partition
    /// (`prompt_cache_key` in the OpenAI API). The harness only carries
    /// it on the wire; the router rewrites Anthropic-shape requests into
    /// OpenAI-native ones and is responsible for actually attaching this
    /// to the OpenAI Responses/Chat API call. Ignored for Anthropic
    /// family because Anthropic's own prompt caching is opt-in via
    /// `cache_control` blocks rather than a routing hint.
    pub prompt_cache_key: Option<String>,
    /// Optional retention hint paired with `prompt_cache_key`. See
    /// [`PromptCacheRetention`] for the wire values.
    pub prompt_cache_retention: Option<PromptCacheRetention>,
    /// Optional request-contract metadata used by provider-bound validation.
    pub metadata: ModelRequestMetadata,
}

impl ModelRequest {
    /// Create a new model request builder.
    #[must_use]
    pub fn builder(model: impl Into<String>, system: impl Into<String>) -> ModelRequestBuilder {
        ModelRequestBuilder::new(model, system)
    }
}

/// Builder for `ModelRequest`.
pub struct ModelRequestBuilder {
    model: String,
    system: String,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
    tool_choice: ToolChoice,
    max_tokens: u32,
    temperature: Option<f32>,
    thinking: Option<ThinkingConfig>,
    auth_token: Option<String>,
    upstream_provider_family: Option<String>,
    aura_project_id: Option<String>,
    aura_agent_id: Option<String>,
    aura_session_id: Option<String>,
    aura_org_id: Option<String>,
    prompt_cache_key: Option<String>,
    prompt_cache_retention: Option<PromptCacheRetention>,
    metadata: ModelRequestMetadata,
}

impl ModelRequestBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new(model: impl Into<String>, system: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            system: system.into(),
            messages: Vec::new(),
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            max_tokens: 4096,
            temperature: None,
            thinking: None,
            auth_token: None,
            upstream_provider_family: None,
            aura_project_id: None,
            aura_agent_id: None,
            aura_session_id: None,
            aura_org_id: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            metadata: ModelRequestMetadata::default(),
        }
    }

    /// Set messages.
    #[must_use]
    pub fn messages(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    /// Add a message.
    #[must_use]
    pub fn message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Set tools.
    #[must_use]
    pub fn tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    /// Set tool choice.
    #[must_use]
    pub fn tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = choice;
        self
    }

    /// Set max tokens.
    #[must_use]
    pub const fn max_tokens(mut self, max: u32) -> Self {
        self.max_tokens = max;
        self
    }

    /// Set temperature.
    #[must_use]
    pub const fn temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    /// Set extended thinking configuration.
    #[must_use]
    pub const fn thinking(mut self, config: ThinkingConfig) -> Self {
        self.thinking = Some(config);
        self
    }

    /// Set the auth token for proxy routing.
    #[must_use]
    pub fn auth_token(mut self, token: Option<String>) -> Self {
        self.auth_token = token;
        self
    }

    /// Set the upstream provider family hint for managed proxy routing.
    #[must_use]
    pub fn upstream_provider_family(mut self, family: Option<String>) -> Self {
        self.upstream_provider_family = family;
        self
    }

    #[must_use]
    pub fn aura_project_id(mut self, id: Option<String>) -> Self {
        self.aura_project_id = id;
        self
    }

    #[must_use]
    pub fn aura_agent_id(mut self, id: Option<String>) -> Self {
        self.aura_agent_id = id;
        self
    }

    #[must_use]
    pub fn aura_session_id(mut self, id: Option<String>) -> Self {
        self.aura_session_id = id;
        self
    }

    #[must_use]
    pub fn aura_org_id(mut self, id: Option<String>) -> Self {
        self.aura_org_id = id;
        self
    }

    #[must_use]
    pub fn prompt_cache_key(mut self, key: Option<String>) -> Self {
        self.prompt_cache_key = key;
        self
    }

    #[must_use]
    pub fn prompt_cache_retention(mut self, retention: Option<PromptCacheRetention>) -> Self {
        self.prompt_cache_retention = retention;
        self
    }

    #[must_use]
    pub const fn request_kind(mut self, kind: ModelRequestKind) -> Self {
        self.metadata.kind = Some(kind);
        self
    }

    #[must_use]
    pub fn metadata(mut self, metadata: ModelRequestMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build the request, validating newtypes at the edge.
    ///
    /// # Errors
    /// Returns [`ReasonerError::Internal`] if any of the newtype invariants
    /// fail: empty model name, `max_tokens == 0`, or temperature outside
    /// `0.0..=2.0`.
    pub fn try_build(self) -> Result<ModelRequest, ReasonerError> {
        let model = ModelName::try_new(self.model)?;
        let max_tokens = MaxTokens::try_new(self.max_tokens)?;
        let temperature = self.temperature.map(Temperature::try_new).transpose()?;
        Ok(ModelRequest {
            model,
            system: self.system,
            messages: self.messages,
            tools: self.tools,
            tool_choice: self.tool_choice,
            max_tokens,
            temperature,
            thinking: self.thinking,
            auth_token: self.auth_token,
            upstream_provider_family: self.upstream_provider_family,
            aura_project_id: self.aura_project_id,
            aura_agent_id: self.aura_agent_id,
            aura_session_id: self.aura_session_id,
            aura_org_id: self.aura_org_id,
            prompt_cache_key: self.prompt_cache_key,
            prompt_cache_retention: self.prompt_cache_retention,
            metadata: self.metadata,
        })
    }
}

#[cfg(test)]
mod newtype_tests {
    use super::*;

    #[test]
    fn model_name_rejects_empty() {
        assert!(ModelName::try_new("").is_err());
        assert!(ModelName::try_new("   ").is_err());
    }

    #[test]
    fn model_name_accepts_valid() {
        let name = ModelName::try_new("claude-opus-4-7").unwrap();
        assert_eq!(name.as_str(), "claude-opus-4-7");
        assert_eq!(format!("{name}"), "claude-opus-4-7");
    }

    #[test]
    fn max_tokens_rejects_zero() {
        assert!(MaxTokens::try_new(0).is_err());
    }

    #[test]
    fn max_tokens_round_trips() {
        let v = MaxTokens::try_new(16_384).unwrap();
        assert_eq!(v.get(), 16_384);
        let raw: u32 = v.into();
        assert_eq!(raw, 16_384);
    }

    #[test]
    fn temperature_rejects_out_of_range() {
        assert!(Temperature::try_new(-0.1).is_err());
        assert!(Temperature::try_new(2.1).is_err());
        assert!(Temperature::try_new(f32::NAN).is_err());
        assert!(Temperature::try_new(f32::INFINITY).is_err());
    }

    #[test]
    fn temperature_accepts_bounds() {
        assert!(Temperature::try_new(0.0).is_ok());
        assert!(Temperature::try_new(2.0).is_ok());
        assert_eq!(Temperature::default(), Temperature::try_new(1.0).unwrap());
    }

    #[test]
    fn try_build_reports_invalid_inputs() {
        let err = ModelRequest::builder("", "sys").try_build().unwrap_err();
        assert!(matches!(err, ReasonerError::Internal(_)));

        let err = ModelRequest::builder("model", "sys")
            .max_tokens(0)
            .try_build()
            .unwrap_err();
        assert!(matches!(err, ReasonerError::Internal(_)));

        let err = ModelRequest::builder("model", "sys")
            .temperature(3.0)
            .try_build()
            .unwrap_err();
        assert!(matches!(err, ReasonerError::Internal(_)));
    }
}
