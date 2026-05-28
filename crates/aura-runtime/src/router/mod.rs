//! HTTP and WebSocket router for the node API.
//!
//! The dispatch root only owns the module declarations and re-exports
//! the public router surface. Implementation lives in dedicated
//! siblings:
//!
//! - [`state`] — [`RouterState`] + [`RouterStateConfig`] and the
//!   `Clone` / `new` plumbing the per-feature handlers thread through.
//! - [`build`] — `create_router` + the middleware-stack assembly,
//!   per-route body limits, governor / CORS / connect-info helpers,
//!   `/health`, `/ws/terminal`.
//! - [`auth`], [`automaton`], [`errors`], [`files`], [`ids`],
//!   [`memory`], [`skills`], [`tool_permissions`], [`tx`], [`ws`] —
//!   per-feature handler bundles already split out before this phase.
//!
//! Sibling modules (and `tests.rs`) pull common dependencies via
//! `use super::*;` — keep the broad `use` block below in sync with
//! whatever those callers expect, so the diff for them stays empty.

#[allow(unused_imports)]
use crate::automaton_bridge::AutomatonBridge;
#[allow(unused_imports)]
use crate::config::NodeConfig;
#[allow(unused_imports)]
use crate::scheduler::Scheduler;
#[allow(unused_imports)]
use crate::session::{handle_chat_ws_connection, WsContext};
#[allow(unused_imports)]
use crate::terminal;
#[allow(unused_imports)]
use aura_core::{Hash, Transaction, TransactionType};
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

mod auth;
mod automaton;
mod build;
mod errors;
mod files;
mod ids;
mod memory;
mod skills;
mod state;
mod tool_permissions;
mod tx;
mod ws;

#[cfg(test)]
mod tests;

pub use build::create_router;
pub use state::{RouterState, RouterStateConfig};
