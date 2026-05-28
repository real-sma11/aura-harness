//! Anthropic provider implementation.
//!
//! Uses `reqwest` directly to communicate with the Anthropic API.
//! This approach is more reliable than SDK wrappers which may have
//! incompatible or private internal types.
//!
//! Supports both synchronous and streaming completions via SSE.

mod api_types;
mod config;
mod convert;
mod provider;
mod sse;

pub use config::{AnthropicConfig, ENV_FALLBACK_MODEL};
pub use provider::exp_backoff_with_jitter;

use crate::error::ReasonerError;
use std::time::Duration;

// ============================================================================
// Internal Error Classification (for retry logic)
// ============================================================================

#[derive(Debug)]
enum ApiError {
    /// 429 / 529 — retryable with backoff, then fallback.
    ///
    /// `retry_after` captures any hint the upstream gave us (either the
    /// HTTP `Retry-After` header or a parsed value from the body). When
    /// present, the retry loop must sleep at least that long before the
    /// next attempt so we do not land inside the rate-limit window again.
    Overloaded {
        message: String,
        retry_after: Option<Duration>,
    },
    /// 402 — stop immediately, no retry or fallback.
    InsufficientCredits(String),
    /// 403 / 503 with managed edge WAF HTML — retryable once so blocks do not storm.
    CloudflareBlock(String),
    /// Generic transient upstream 5xx — 500 / 502 / 503 (non-Cloudflare) /
    /// 504. Mapped to a retryable class so a single provider blip doesn't
    /// immediately surface a terminal `LLM error: ...` to the dev loop.
    /// When retries are exhausted this falls back to
    /// [`ReasonerError::Transient`] via the `From<ApiError>` impl —
    /// callers that branch on retry classification can match the
    /// dedicated variant rather than predicating over the status range,
    /// while existing `Display` text is unchanged.
    TransientServer { status: u16, message: String },
    /// Any other failure.
    Other(ReasonerError),
}

impl From<ApiError> for ReasonerError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Overloaded {
                message,
                retry_after,
            } => Self::RateLimited {
                message: format_rate_limited_message(&message, retry_after),
                retry_after,
            },
            ApiError::InsufficientCredits(msg) => Self::InsufficientCredits(msg),
            // Managed edge WAF blocks are transient — encode that
            // classification in the variant rather than expecting
            // downstream code to special-case status 403 messages.
            ApiError::CloudflareBlock(msg) => Self::Transient {
                status: 403,
                message: msg,
                retry_after: None,
            },
            ApiError::TransientServer { status, message } => Self::Transient {
                status,
                message,
                retry_after: None,
            },
            ApiError::Other(e) => e,
        }
    }
}

/// Format a rate-limited message that always surfaces a `retry after N seconds`
/// hint when one was reported by the upstream. The proxy's own body message
/// usually already contains this phrasing, so we avoid duplicating it.
fn format_rate_limited_message(message: &str, retry_after: Option<Duration>) -> String {
    match retry_after {
        Some(delay) if !message.to_ascii_lowercase().contains("retry after") => {
            format!("{message} (retry after {} seconds)", delay.as_secs().max(1))
        }
        _ => message.to_string(),
    }
}

fn is_cloudflare_html(body: &str) -> bool {
    if !body.contains("<!DOCTYPE html") {
        return false;
    }

    let body_lower = body.to_ascii_lowercase();
    body_lower.contains("cloudflare")
        || body_lower.contains("oldie")
        || body_lower.contains("web application firewall")
        || body_lower.contains("your request was blocked")
}

// ============================================================================
// Provider Implementation
// ============================================================================

/// Anthropic model provider.
///
/// Implements `ModelProvider` for the Anthropic API using direct HTTP calls.
/// Includes built-in retry with exponential backoff for overloaded (429/529)
/// errors and automatic fallback to a secondary model.
pub struct AnthropicProvider {
    client: reqwest::Client,
    config: AnthropicConfig,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider.
    ///
    /// # Errors
    ///
    /// Returns error if client creation fails.
    pub fn new(config: AnthropicConfig) -> Result<Self, ReasonerError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_millis(config.timeout_ms))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| ReasonerError::Internal(format!("HTTP client creation failed: {e}")))?;
        Ok(Self { client, config })
    }

    /// Create from environment variables.
    ///
    /// # Errors
    ///
    /// Returns error if HTTP client creation fails.
    pub fn from_env() -> Result<Self, ReasonerError> {
        Self::new(AnthropicConfig::from_env())
    }

    /// Build the ordered model fallback chain.
    pub(crate) fn model_chain(&self, primary: &str) -> Vec<String> {
        let mut models = vec![primary.to_string()];
        if let Some(ref fb) = self.config.fallback_model {
            if fb != primary {
                models.push(fb.clone());
            }
        }
        models
    }
}

#[cfg(test)]
mod tests;
