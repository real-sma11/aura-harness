//! # aura-core-auth
//!
//! Layer: core
//!
//! Pure auth primitive types only — no network, no storage, no
//! provider implementations. The login flow and credential persistence
//! live in `aura-auth` (which re-exports the types below for source
//! compatibility) and will move to `aura-surface-auth` in a later
//! phase.
//!
//! ## Invariants
//!
//! - [`StoredSession`] serialises exactly the same JSON as the legacy
//!   `aura_auth::StoredSession` — moving it here is wire-compatible.
//! - [`AccessToken`] / [`RefreshToken`] are opaque strings; the crate
//!   has no opinion on format (JWT vs. opaque) — that lives in the
//!   provider implementation.
//!
//! ## Failure modes
//!
//! - [`AuthError`] is a closed taxonomy of primitive failure modes
//!   that any auth backend can produce without dragging in
//!   network/keyring dependencies. Backends layer richer error
//!   variants on top via `#[from]`.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use aura_core_types::UserId;

/// Errors produced by pure auth primitives.
///
/// Network/keyring/io variants live on top of this in backend crates
/// via `#[from]` or wrapping.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Serialisation/deserialisation of credential data failed.
    #[error("credential serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// Backend returned a structured rejection from the upstream auth
    /// provider.
    #[error("auth provider rejected the request (status {status}): {message}")]
    Rejected {
        /// HTTP-style status code returned by the provider.
        status: u16,
        /// Provider-specific error code.
        code: String,
        /// Human-readable message.
        message: String,
    },
    /// The provider returned a response that did not match the
    /// expected shape.
    #[error("malformed auth response: {0}")]
    Malformed(String),
    /// Local storage backend reported an unrecoverable error.
    #[error("auth storage error: {0}")]
    Storage(String),
}

/// An opaque access token issued by the upstream auth provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessToken(pub String);

impl AccessToken {
    /// Return the inner string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A refresh token, when issued by the provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshToken(pub String);

impl RefreshToken {
    /// Return the inner string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Either kind of token, tagged by kind on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Token {
    /// An access token.
    Access {
        /// Raw token value.
        value: String,
    },
    /// A refresh token.
    Refresh {
        /// Raw token value.
        value: String,
    },
}

/// Persisted authentication session.
///
/// Wire-compatible with the legacy `aura_auth::StoredSession`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    /// JWT access token for the upstream proxy.
    pub access_token: String,
    /// Upstream user id.
    pub user_id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Primary zID (e.g. `0://alice`).
    pub primary_zid: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_session_roundtrips() {
        let session = StoredSession {
            access_token: "tok".into(),
            user_id: "u".into(),
            display_name: "n".into(),
            primary_zid: "0://x".into(),
            created_at: Utc::now(),
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: StoredSession = serde_json::from_str(&json).unwrap();
        assert_eq!(back.user_id, session.user_id);
    }

    #[test]
    fn token_tagged_kind_serialisation() {
        let t = Token::Access { value: "x".into() };
        let json = serde_json::to_value(&t).unwrap();
        assert_eq!(json["kind"], "access");
        assert_eq!(json["value"], "x");
    }
}
