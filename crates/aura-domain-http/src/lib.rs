//! # aura-domain-http
//!
//! Layer: surface
//!
//! HTTP-backed [`aura_tools::domain_tools::DomainApi`] implementation
//! plus the JWT-injecting wrapper the automaton bridge uses to stamp
//! a captured token onto every call site that did not supply one.
//!
//! Phase C / Commit 4 moves these two files out of `aura-runtime` so
//! the gateway crate no longer owns domain HTTP logic or depends on
//! `reqwest` for domain calls. Both crates sit at the surface layer:
//! this crate composes lower-layer types
//! ([`aura_tools::domain_tools::DomainApi`]) into a deployable HTTP
//! edge, and the gateway composes both this crate and the engine into
//! a runnable HTTP/WS server. `aura-runtime` still has separate direct
//! outbound HTTP surfaces (for example the generation proxy and
//! cross-agent callback path).
//!
//! ## Surface
//!
//! - [`HttpDomainApi`] — the `reqwest`-backed `DomainApi` impl with
//!   Cloudflare-block retry handling, `aura-os-server` base-URL
//!   override routing, and JWT-bearer authentication.
//! - [`JwtDomainApi`] — wraps another `DomainApi` and injects a
//!   captured JWT on every call site that did not supply one.
//!
//! ## Error handling
//!
//! The current `DomainApi` trait returns `anyhow::Result`, so both
//! impls also surface `anyhow::Error`. A future commit can introduce
//! a typed `DomainError` once the trait migrates off `anyhow`.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
#![allow(
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

mod http;
mod jwt;

pub use http::HttpDomainApi;
pub use jwt::JwtDomainApi;
