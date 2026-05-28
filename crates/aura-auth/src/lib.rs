//! # aura-auth
//!
//! Layer: surface
//!
//! Authentication client and credential storage for the Aura CLI.
//!
//! Phase 9 reclassifies this crate at the surface layer: the
//! network client (`ZosClient`) and the OS-keyring/file
//! credential persistence (`CredentialStore`) are surface-side
//! concerns per the plan's "Secrets" bullet (§5 cross-cutting
//! ownership). Token primitive types (`AccessToken`,
//! `RefreshToken`, `Token`, `StoredSession`, `AuthError`) remain
//! at the core layer in `aura-core-auth` and are re-exported here
//! for source compatibility.
//!
//! Provides:
//! - [`ZosClient`] for authenticating against the zOS API (`zosapi.zero.tech`)
//! - [`CredentialStore`] for persisting JWT tokens to `~/.aura/credentials.json`
//! - [`StoredSession`] (re-exported from `aura-core-auth`) as the
//!   serializable session type
//!
//! The pure auth primitives ([`StoredSession`], the
//! [`AccessToken`]/[`RefreshToken`]/[`Token`] enum, and a primitive
//! [`PrimitiveAuthError`]) live in `aura-core-auth` and are
//! re-exported here for source compatibility. Backend-flavoured
//! variants of [`AuthError`] (HTTP + keyring) stay in this crate.
//!
//! # Login flow
//!
//! 1. Prompt the user for email and password.
//! 2. Call [`ZosClient::login`] to obtain a JWT access token.
//! 3. Call [`CredentialStore::save`] to persist the session to disk.
//! 4. The JWT is then available via [`CredentialStore::load_token`] for proxy
//!    mode requests.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod credentials;
mod error;
mod redact;
mod zos_client;

pub use credentials::{CredentialStore, StoredSession};
pub use error::AuthError;
pub use redact::redact_error;
pub use zos_client::ZosClient;

// Re-export the pure primitive types from aura-core-auth so callers
// can pull token primitives without depending on the backend crate.
pub use aura_core_auth::{AccessToken, AuthError as PrimitiveAuthError, RefreshToken, Token};
