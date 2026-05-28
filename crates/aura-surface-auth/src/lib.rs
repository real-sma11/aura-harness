//! # aura-surface-auth
//!
//! Layer: surface
//!
//! Phase 9 relocation shell for the zOS HTTP client and credential
//! storage. The token primitive types (`AccessToken`,
//! `RefreshToken`, `Token`, `StoredSession`, `AuthError`) stay at
//! the `core` layer in `aura-core-auth`; the network client and
//! the OS-keyring/file credential persistence (i.e. side-effectful
//! code with reqwest + keyring dependencies) belongs at the
//! surface layer.
//!
//! This crate re-exports the [`aura_auth::ZosClient`] HTTP client,
//! the [`aura_auth::CredentialStore`] keyring-backed credential
//! persistence, and the bigger [`aura_auth::AuthError`] enum so
//! callers can pull in a single surface-layer crate for all login
//! / logout / whoami needs.
//!
//! Migration note: the underlying `aura-auth` crate keeps the
//! existing module names so the workspace continues to build
//! without touching every import site; new code should pull the
//! types from `aura_surface_auth::*` instead.
//!
//! ## Invariants ([`.cursor/rules.md`] §13)
//!
//! - The keyring / file credential persistence MUST stay isolated
//!   to the surface layer per the Phase 1 plan (§5 cross-cutting
//!   ownership, "Secrets" bullet). Library crates consume
//!   [`aura_core_auth::Token`] / [`aura_core_auth::StoredSession`]
//!   only.
//! - No network calls are issued from anywhere outside this crate
//!   or its dependency `aura-auth`; `aura-runtime` and `aura-agent`
//!   never speak HTTP to zOS directly.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_auth::{AuthError, CredentialStore, ZosClient};
pub use aura_core_auth::{AccessToken, RefreshToken, StoredSession, Token};
