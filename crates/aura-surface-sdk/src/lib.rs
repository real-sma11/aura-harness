//! # aura-surface-sdk
//!
//! Layer: surface
//!
//! External SDK shape for talking to an Aura fleet daemon over the
//! [`aura_core_protocol`] wire types. Phase 9 introduces the SDK
//! surface crate; the actual transport (HTTP / WebSocket / in-proc)
//! is supplied by surface-layer composition crates (e.g.
//! `aura-surface-cli` embeds an in-process daemon) and is NOT
//! pinned in this crate.
//!
//! The SDK exposes three primary types:
//!
//! - [`AuraClient`] — a thin handle to a daemon (created from
//!   [`AuraClientConfig`]). The actual transport is plug-in via the
//!   [`SessionTransport`] trait so the consumer can build a CLI
//!   embedded client, a remote HTTP client, or a unit-test fixture.
//! - [`AuraSession`] — one open session against the daemon, holding
//!   the resolved [`AgentMode`] and the transport handle.
//! - [`SessionConfig`] — caller-supplied session configuration; the
//!   `mode: Option<AgentMode>` field is the SDK input into the
//!   Phase 9 mode resolution priority.
//!
//! ## Mode resolution priority
//!
//! The SDK is one of four inputs in the documented Phase 9
//! priority chain. See [`aura_core_modes`] and
//! `aura_fleet_daemon::resolve_session_mode` for the resolution
//! semantics — the SDK contributes via
//! [`SessionConfig::mode`].
//!
//! ## Invariants ([`.cursor/rules.md`] §13)
//!
//! - No transport dependency — the surface-layer composition root
//!   owns the wire (HTTP / WebSocket / in-process). This keeps the
//!   SDK type surface stable as transports evolve.
//! - The SDK never panics on a daemon-side error; everything
//!   surfaces as a typed [`SdkError`].

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use aura_core_modes::AgentMode;
use aura_core_protocol::ProtocolVersion;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors raised by the SDK surface.
#[derive(Debug, Error)]
pub enum SdkError {
    /// The supplied [`AuraClientConfig`] is invalid (e.g. empty
    /// endpoint).
    #[error("invalid client config: {0}")]
    InvalidConfig(String),
    /// The transport layer failed to open or relay the request.
    #[error("transport error: {0}")]
    Transport(String),
    /// The daemon-side handler returned an error payload.
    #[error("daemon error: {0}")]
    Daemon(String),
}

/// Caller-supplied client configuration.
///
/// Used to build an [`AuraClient`] via [`AuraClient::new`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuraClientConfig {
    /// Endpoint string. Implementation-defined: a `tcp://host:port`
    /// for a remote daemon, an `inproc://` URI for an embedded
    /// daemon, or a filesystem socket path. Empty strings are
    /// rejected by [`AuraClient::new`].
    pub endpoint: String,
    /// Optional client name used for telemetry / audit
    /// attribution. Pass-through to the daemon.
    #[serde(default)]
    pub client_name: Option<String>,
    /// Wire protocol pinned by the caller. Defaults to the
    /// [`aura_core_protocol::PROTOCOL_VERSION`] constant.
    #[serde(default = "default_protocol_version")]
    pub protocol: ProtocolVersion,
}

fn default_protocol_version() -> ProtocolVersion {
    aura_core_protocol::PROTOCOL_VERSION
}

impl Default for AuraClientConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            client_name: None,
            protocol: aura_core_protocol::PROTOCOL_VERSION,
        }
    }
}

/// Caller-supplied per-session configuration.
///
/// The `mode` field is the SDK input into the documented
/// AgentMode resolution priority (CLI flag > TUI slash > **SDK
/// field** > daemon default > [`AgentMode::Agent`] fallback).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Caller-pinned [`AgentMode`]. Wins over the daemon default
    /// but loses to a CLI flag or TUI `/mode` slash command.
    ///
    /// `None` means "no SDK override — defer to the next priority
    /// rung (daemon default, then `AgentMode::Agent` fallback)".
    pub mode: Option<AgentMode>,
    /// Optional caller-supplied tag passed to the daemon (audit
    /// attribution, telemetry labels). No semantic meaning to the
    /// daemon itself.
    #[serde(default)]
    pub tag: Option<String>,
}

/// Pluggable session transport.
///
/// The SDK does not own a transport — callers wire one of the
/// surface-layer transports (`aura-surface-cli` embeds an
/// in-process daemon; other implementations may speak HTTP /
/// WebSocket / Unix socket). Implementors must be `Send + Sync` so
/// the resulting [`AuraSession`] can move across threads.
pub trait SessionTransport: Send + Sync {
    /// Send a one-shot prompt and return the daemon's reply.
    ///
    /// Streaming is layered on top of this surface in transport
    /// implementations — the SDK only requires a synchronous
    /// `prompt → response` shape so a smoke-test fixture can
    /// implement it in five lines.
    ///
    /// # Errors
    ///
    /// Implementations should surface transport-layer failures as
    /// [`SdkError::Transport`] and daemon-side handler errors as
    /// [`SdkError::Daemon`].
    fn prompt(&self, body: &str) -> Result<String, SdkError>;
}

/// A handle to a daemon. Cheap to clone (an `Arc` wrapping the
/// transport object).
pub struct AuraClient {
    config: AuraClientConfig,
    transport: std::sync::Arc<dyn SessionTransport>,
}

impl AuraClient {
    /// Build a client from a config and a transport.
    ///
    /// # Errors
    ///
    /// Returns [`SdkError::InvalidConfig`] when the endpoint is an
    /// empty string.
    pub fn new(
        config: AuraClientConfig,
        transport: std::sync::Arc<dyn SessionTransport>,
    ) -> Result<Self, SdkError> {
        if config.endpoint.is_empty() {
            return Err(SdkError::InvalidConfig(
                "endpoint must be non-empty".to_string(),
            ));
        }
        Ok(Self { config, transport })
    }

    /// Read-only view of the active client config.
    #[must_use]
    pub fn config(&self) -> &AuraClientConfig {
        &self.config
    }

    /// Open a session with the supplied [`SessionConfig`]. The
    /// returned [`AuraSession`] carries the **caller-side** mode
    /// preference; the daemon applies the documented resolution
    /// priority server-side and the final resolved mode is exposed
    /// on the daemon-side session record (out of scope for this
    /// SDK type).
    #[must_use]
    pub fn open_session(&self, config: SessionConfig) -> AuraSession {
        AuraSession {
            transport: self.transport.clone(),
            config,
        }
    }
}

/// One open session.
///
/// Cheap to drop; the underlying transport is reference-counted.
pub struct AuraSession {
    transport: std::sync::Arc<dyn SessionTransport>,
    config: SessionConfig,
}

impl AuraSession {
    /// Read-only view of the session config used to open this
    /// session.
    #[must_use]
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// Send a one-shot prompt and return the daemon reply.
    ///
    /// # Errors
    ///
    /// Bubbles up [`SdkError`] from the underlying transport.
    pub fn prompt(&self, body: &str) -> Result<String, SdkError> {
        self.transport.prompt(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct EchoTransport;
    impl SessionTransport for EchoTransport {
        fn prompt(&self, body: &str) -> Result<String, SdkError> {
            Ok(format!("echo: {body}"))
        }
    }

    #[test]
    fn rejects_empty_endpoint() {
        let cfg = AuraClientConfig::default();
        assert!(AuraClient::new(cfg, Arc::new(EchoTransport)).is_err());
    }

    #[test]
    fn smoke_echo_round_trip() {
        let cfg = AuraClientConfig {
            endpoint: "inproc://fixture".to_string(),
            ..AuraClientConfig::default()
        };
        let client = AuraClient::new(cfg, Arc::new(EchoTransport)).expect("valid endpoint");
        let session = client.open_session(SessionConfig {
            mode: Some(AgentMode::Plan),
            tag: Some("smoke".to_string()),
        });
        let reply = session.prompt("hello").expect("echo transport never fails");
        assert_eq!(reply, "echo: hello");
        assert_eq!(session.config().mode, Some(AgentMode::Plan));
    }
}
