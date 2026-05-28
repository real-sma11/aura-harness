//! Typed errors for the reasoner crate.
//!
//! [`ReasonerError`] classifies provider failures so that retry and fallback
//! logic can branch on the error *variant* rather than string-matching status
//! codes embedded in the error message.

use crate::types::{ModelRequestContractViolation, PartialToolUse};
use std::time::Duration;

/// Classified model-provider error.
///
/// Returned from [`ModelProvider::complete`](crate::ModelProvider::complete) and
/// other provider implementations. Consumers can match on the variant directly.
///
/// ## Phase 5 polish
///
/// `RateLimited` now carries a structured `retry_after: Option<Duration>`
/// (lifted out of the previously-stringified message) and a new
/// [`Self::Transient`] variant captures retryable upstream 5xx so that
/// callers don't have to re-derive retry classification by matching on
/// HTTP status integers or parsing a free-form message. The [`std::fmt::Display`]
/// rendering of `RateLimited` is unchanged (`"Rate limited: {message}"`)
/// so log-parsing tooling that greps for that literal still works.
#[derive(Debug, thiserror::Error)]
pub enum ReasonerError {
    /// 429 / 529 — the provider is rate-limiting or overloaded.
    /// Eligible for exponential backoff and model fallback. The
    /// optional `retry_after` is the duration the provider asked us to
    /// wait (via the `Retry-After` header or a parsed body field) and
    /// is also embedded in `message` for human-readable logging.
    #[error("Rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },

    /// 402 — insufficient credits. Must stop immediately.
    #[error("Insufficient credits: {0}")]
    InsufficientCredits(String),

    /// Retryable upstream 5xx (500/502/503/504, plus Cloudflare cold
    /// starts). Distinguished from [`Self::Api`] so retry/backoff
    /// helpers can match on a single variant rather than predicating
    /// over a status range. `retry_after` mirrors `RateLimited` and
    /// captures any `Retry-After` hint the upstream sent.
    #[error("API error (status {status}): {message}")]
    Transient {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
    },

    /// HTTP-level API error with status code (catch-all for
    /// non-retryable 4xx and unclassified failures). Typed retry logic
    /// should prefer [`Self::Transient`] for retryable codes — leaving
    /// this variant for terminal "bad request" / "auth" / "not found"
    /// classes.
    #[error("API error (status {status}): {message}")]
    Api { status: u16, message: String },

    /// Network or connection-level failure.
    #[error("request error: {0}")]
    Request(String),

    /// Request timed out.
    #[error("timeout")]
    Timeout,

    /// Failed to parse a response body.
    #[error("parse error: {0}")]
    Parse(String),

    /// A streaming response was interrupted mid-flight by a transport
    /// or SSE-level error while a `tool_use` block was still being
    /// accumulated.
    ///
    /// Carries enough context for the agent-loop to drive a
    /// per-tool-call retry (re-issuing a fresh streaming request) rather
    /// than silently fall back to a non-streaming call that would have
    /// no memory of the interrupted tool call. Returned from
    /// [`crate::types::StreamAccumulator::into_response`] when
    /// `stream_error` is set; the caller is responsible for deciding
    /// whether to retry or propagate.
    #[error("{reason}")]
    StreamAbortedWithPartial {
        /// Human-readable reason, already annotated with
        /// `model=... msg_id=... request_id=...` context when
        /// available.
        reason: String,
        /// In-flight tool-use captured just before the stream died.
        /// `None` when the error arrived before `content_block_start`.
        partial_tool_use: Option<PartialToolUse>,
    },

    /// Provider-bound request failed the local harness contract before it was
    /// sent to the router.
    ///
    /// Boxed because [`ModelRequestContractViolation`] embeds a full
    /// [`crate::ModelContentProfile`] (multiple `Vec<String>` plus a
    /// 16-char hash signature). Holding it inline pushed every
    /// [`ReasonerError`] above the 128-byte
    /// `result_large_err`/`Result<_, ReasonerError>` ceiling and
    /// poisoned every fallible provider helper. Boxing isolates the
    /// allocation on the rare contract-violation path.
    #[error("{0}")]
    ModelRequestContractViolation(Box<ModelRequestContractViolation>),

    /// Catch-all for other provider-level failures.
    #[error("{0}")]
    Internal(String),
}

impl ReasonerError {
    #[must_use]
    pub const fn is_insufficient_credits(&self) -> bool {
        matches!(self, Self::InsufficientCredits(_))
    }

    #[must_use]
    pub fn is_context_overflow(&self) -> bool {
        match self {
            Self::Api { status, message }
            | Self::Transient {
                status, message, ..
            } => {
                *status == 413
                    || ((*status == 400 || *status == 422)
                        && message_indicates_context_overflow(message))
            }
            Self::Request(message) | Self::Parse(message) | Self::Internal(message) => {
                message_indicates_context_overflow(message)
            }
            Self::RateLimited { .. }
            | Self::InsufficientCredits(_)
            | Self::Timeout
            | Self::StreamAbortedWithPartial { .. }
            | Self::ModelRequestContractViolation(_) => false,
        }
    }

    /// Hint at how long the caller should wait before retrying. Set
    /// for [`Self::RateLimited`] and [`Self::Transient`] when the
    /// upstream supplied a `Retry-After` value; `None` otherwise.
    #[must_use]
    pub const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } | Self::Transient { retry_after, .. } => {
                *retry_after
            }
            _ => None,
        }
    }

    /// Returns true if the error is in a class that the agent loop /
    /// retry helper should treat as transient (rate-limit, overloaded,
    /// retryable upstream 5xx). Centralises the classification so
    /// callers don't string-match HTTP status codes.
    #[must_use]
    pub const fn is_retryable_transient(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient { .. })
    }
}

fn message_indicates_context_overflow(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    [
        "prompt is too long",
        "prompt too long",
        "prompt too large",
        "context length exceeded",
        "context window exceeded",
        "context window limit",
        "exceeds context window",
        "exceed the model context window",
        "input length and max_tokens exceed context limit",
        "requested tokens exceed the context window",
        "request exceeds the context window",
        "too many tokens",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    #[test]
    fn test_rate_limited_display() {
        let err = ReasonerError::RateLimited {
            message: "429 too many requests".to_string(),
            retry_after: None,
        };
        let msg = format!("{err}");
        assert_eq!(msg, "Rate limited: 429 too many requests");
    }

    #[test]
    fn test_rate_limited_retry_after_field_preserved() {
        let err = ReasonerError::RateLimited {
            message: "rate limit: retry after 7 seconds".to_string(),
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(err.retry_after(), Some(Duration::from_secs(7)));
        assert!(err.is_retryable_transient());
    }

    #[test]
    fn test_transient_classification() {
        let err = ReasonerError::Transient {
            status: 502,
            message: "bad gateway".to_string(),
            retry_after: None,
        };
        assert!(err.is_retryable_transient());
        assert_eq!(format!("{err}"), "API error (status 502): bad gateway");
    }

    #[test]
    fn test_api_error_not_retryable_transient() {
        let err = ReasonerError::Api {
            status: 401,
            message: "unauthorized".to_string(),
        };
        assert!(!err.is_retryable_transient());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn test_insufficient_credits_display() {
        let err = ReasonerError::InsufficientCredits("402 payment required".to_string());
        let msg = format!("{err}");
        assert_eq!(msg, "Insufficient credits: 402 payment required");
    }

    #[test]
    fn test_api_error_display() {
        let err = ReasonerError::Api {
            status: 500,
            message: "internal error".to_string(),
        };
        let msg = format!("{err}");
        assert_eq!(msg, "API error (status 500): internal error");
    }

    #[test]
    fn test_request_error_display() {
        let err = ReasonerError::Request("connection refused".to_string());
        assert_eq!(format!("{err}"), "request error: connection refused");
    }

    #[test]
    fn test_timeout_display() {
        let err = ReasonerError::Timeout;
        assert_eq!(format!("{err}"), "timeout");
    }

    #[test]
    fn test_parse_error_display() {
        let err = ReasonerError::Parse("invalid JSON".to_string());
        assert_eq!(format!("{err}"), "parse error: invalid JSON");
    }

    #[test]
    fn test_internal_error_display() {
        let err = ReasonerError::Internal("something broke".to_string());
        assert_eq!(format!("{err}"), "something broke");
    }

    #[test]
    fn test_downcast_from_anyhow() {
        let err: anyhow::Error = ReasonerError::RateLimited {
            message: "429".to_string(),
            retry_after: None,
        }
        .into();
        let downcasted = err.downcast_ref::<ReasonerError>();
        assert!(downcasted.is_some());
        assert!(matches!(
            downcasted.unwrap(),
            ReasonerError::RateLimited { .. }
        ));
    }

    #[test]
    fn test_downcast_insufficient_credits() {
        let err: anyhow::Error = ReasonerError::InsufficientCredits("402".to_string()).into();
        let downcasted = err.downcast_ref::<ReasonerError>();
        assert!(matches!(
            downcasted,
            Some(ReasonerError::InsufficientCredits(_))
        ));
    }

    #[test]
    fn test_debug_formatting() {
        let err = ReasonerError::Internal("bad request".to_string());
        let mut buf = String::new();
        write!(&mut buf, "{err:?}").unwrap();
        assert!(buf.contains("Internal"));
        assert!(buf.contains("bad request"));
    }

    #[test]
    fn test_context_overflow_detection_for_api_error() {
        let err = ReasonerError::Api {
            status: 400,
            message: "input length and max_tokens exceed context limit".to_string(),
        };
        assert!(err.is_context_overflow());
    }

    #[test]
    fn test_context_overflow_detection_for_413() {
        let err = ReasonerError::Api {
            status: 413,
            message: "request entity too large".to_string(),
        };
        assert!(err.is_context_overflow());
    }

    #[test]
    fn test_context_overflow_detection_ignores_other_api_errors() {
        let err = ReasonerError::Api {
            status: 400,
            message: "invalid api key".to_string(),
        };
        assert!(!err.is_context_overflow());
    }
}
