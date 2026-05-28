//! Thin re-export layer over [`aura_agent::session_bootstrap`].
//!
//! Phase 3 consolidated every non-TUI-specific helper into the library
//! crate so `aura-runtime`, the TUI harness, and any future embedder read
//! the same env-var / policy / executor wiring. This file used to own
//! ~125 lines of that code; it now just re-exports the canonical
//! versions. New helpers should land in
//! [`aura_agent::session_bootstrap`] directly rather than here.
//!
//! Phase 9 surface-layer narrowing: credential storage now lives in
//! [`aura_auth::CredentialStore`] (surface) only. The agent-layer
//! `load_auth_token` returns the env-var only; surface-layer
//! composition (here) chains the credential store as a fallback so
//! existing `aura login` flows continue to feed the agent loop.

#[allow(unused_imports)]
pub use aura_agent::session_bootstrap::{
    build_executor_router_with_config, default_agent_config_with_auth, load_auth_token, open_store,
    resolve_store_path,
};

/// Phase 9 surface-layer wrapper around
/// [`aura_agent::session_bootstrap::default_agent_config_with_auth`].
///
/// Composes the env-var lookup (from `aura-agent`) with the
/// credential-store lookup (from `aura-auth`) before handing the
/// resolved `Option<String>` to the agent-layer helper. The
/// previous `default_agent_config(model)` shape is preserved as a
/// pass-through so existing callers see no behaviour change.
#[must_use]
pub fn default_agent_config(model: impl Into<String>) -> aura_agent::AgentLoopConfig {
    let auth_token = aura_agent::session_bootstrap::load_auth_token()
        .or_else(aura_auth::CredentialStore::load_token);
    default_agent_config_with_auth(model, auth_token)
}
