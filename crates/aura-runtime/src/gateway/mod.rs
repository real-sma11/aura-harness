//! HTTP/WS gateway for the `aura-node` binary.
//!
//! Phase C / Commit 4 renamed the former `router/` module to
//! `gateway/` and grouped per-endpoint handlers under `handlers/`.
//! Layout:
//!
//! - [`state`] — [`RouterState`] + [`RouterStateConfig`] threaded
//!   through every handler.
//! - [`middleware`] — `create_router`, the middleware-stack assembly
//!   (CORS / governor / body-limit / connect-info / observer /
//!   timeout / trace), the terminal-upgrade delegate, and the
//!   `/health` handler. Split out of the old `router/build.rs`.
//! - [`handlers`] — per-endpoint handler bundles:
//!   [`handlers::run`] (`POST /v1/run`, `/v1/run/list`,
//!   `/v1/run/:id/{status,pause,stop}`), [`handlers::files`],
//!   [`handlers::tx`], [`handlers::memory`], [`handlers::skills`],
//!   [`handlers::tool_permissions`], [`handlers::run_ws`]
//!   (`WS /stream/:run_id`).
//! - [`session`] — per-WebSocket-connection state: chat-session
//!   bootstrap, tool-approval broker, partial-JSON repair.
//! - [`auth_mw`] — bearer-token enforcing axum middleware shared
//!   across protected routes.
//! - [`errors`] — `ApiError`, the canonical JSON failure shape.
//!
//! Sibling modules pull common dependencies via `use super::*;` —
//! keep the broad `use` block below in sync with whatever those
//! callers expect, so the diff for them stays empty.

#[allow(unused_imports)]
use crate::config::NodeConfig;
#[allow(unused_imports)]
use crate::gateway::session::WsContext;
#[allow(unused_imports)]
use crate::terminal;
#[allow(unused_imports)]
use aura_core::{Hash, Transaction, TransactionType};
#[allow(unused_imports)]
use aura_engine::automaton::AutomatonBridge;
#[allow(unused_imports)]
use aura_engine::scheduler::Scheduler;
#[allow(unused_imports)]
use aura_reasoner::ModelProvider;
#[allow(unused_imports)]
use aura_store::Store;
#[allow(unused_imports)]
use aura_tools::automaton_tools::AutomatonController;
#[allow(unused_imports)]
use aura_tools::domain_tools::DomainApi;
#[allow(unused_imports)]
use aura_tools::{ToolCatalog, ToolConfig};
#[allow(unused_imports)]
use axum::{
    extract::{ws::WebSocketUpgrade, DefaultBodyLimit, Path, Query, State},
    http::{
        header::{self, HeaderName},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
#[allow(unused_imports)]
use bytes::Bytes;
#[allow(unused_imports)]
use dashmap::DashMap;
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use std::net::SocketAddr;
#[allow(unused_imports)]
use std::sync::{Arc, RwLock};
#[allow(unused_imports)]
use std::time::Duration;
#[allow(unused_imports)]
use tokio::sync::Semaphore;
#[allow(unused_imports)]
use tower::limit::GlobalConcurrencyLimitLayer;
#[allow(unused_imports)]
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
#[allow(unused_imports)]
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    timeout::TimeoutLayer,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
#[allow(unused_imports)]
use tracing::{error, info, instrument, warn, Level};

pub(crate) mod auth_mw;
pub(crate) mod errors;
pub(crate) mod handlers;
pub(crate) mod middleware;
pub(crate) mod session;
pub(crate) mod state;

#[cfg(test)]
mod tests;

pub use middleware::create_router;
pub use state::{RouterState, RouterStateConfig};
