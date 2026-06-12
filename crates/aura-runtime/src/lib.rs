//! # aura-runtime
//!
//! Layer: surface
//!
//! HTTP/WS gateway crate for Aura. Hosts the canonical
//! [`aura_protocol::RuntimeRequest`] entry endpoints (`POST /v1/run`,
//! `WS /stream/:run_id`) and the management surfaces (skills, memory,
//! tool defaults, files, transactions). Composes
//! [`aura_engine`] for orchestration and
//! [`aura_fleet_subagent::FleetSubagentDispatcher`] for the `task`
//! tool's child-spawn surface.
//!
//! The compiled binary name (`aura-node`) is declared in
//! `Cargo.toml`'s `[[bin]]` section and is deliberately decoupled
//! from the crate name to avoid churn in Dockerfile `CMD` and
//! operator scripts.
//!
//! Phase B / Commit 3 extracted the orchestration engine into the
//! `aura-engine` crate. The scheduler, worker, automaton bridge,
//! memory observer, runtime capabilities, executor factory, and
//! `RuntimeChildRunner` live there now. The subagent dispatcher
//! impl moved to the new `aura-fleet-subagent` crate (fleet layer);
//! the bundled registry + pure-data adapter helpers moved to
//! `aura-agent-subagent` (agent layer).
//!
//! Phase C / Commit 4 pulled the HTTP `DomainApi` impl into the
//! `aura-domain-http` crate (along with the JWT-injecting wrapper
//! that Phase B had parked in `aura-engine`) and lifted the PTY
//! WebSocket handler into `aura-terminal`. The former
//! `crate::router` module was renamed to `crate::gateway` and
//! reorganized into a handler-grouped layout
//! (`gateway::handlers::{run, run_ws, files, tx, memory, skills,
//! tool_permissions}`) plus `gateway::session::*` for the
//! per-WebSocket-connection protocol layer; the middleware-stack
//! assembly lives in `gateway::middleware::create_router`.
//!
//! Provides:
//! - HTTP gateway for `POST /v1/run` + management endpoints.
//! - WS handlers attached to a [`aura_engine::Scheduler`].
//! - Auth, config, files-api helpers shared with surface-cli.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
#![allow(
    clippy::doc_markdown,
    clippy::must_use_candidate,
    clippy::match_same_arms,
    clippy::single_match,
    clippy::single_match_else,
    clippy::option_if_let_else,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::unnecessary_map_or,
    clippy::wildcard_imports,
    clippy::manual_let_else,
    clippy::ignored_unit_patterns,
    clippy::significant_drop_tightening,
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::unused_self,
    clippy::struct_field_names
)]

pub mod auth;
mod config;
pub mod console_format;
pub mod files_api;
pub(crate) mod gateway;
pub mod inbound_console;
mod node;
pub(crate) mod protocol;
pub mod sealing;
pub mod trigger_registrar;
pub(crate) mod tool_permissions;
/// Phase C / Commit 4: legacy `crate::terminal::handle_terminal_ws`
/// import path inside the gateway points at this re-export so the
/// terminal-upgrade handler keeps reading naturally even though the
/// implementation now lives in `aura-terminal`.
pub(crate) mod terminal {
    pub(crate) use aura_terminal::ws::handle_terminal_ws;
}

pub use config::NodeConfig;
pub use node::Node;

/// Re-exports of orchestration primitives the gateway composes.
///
/// External consumers reach into `aura_runtime::scheduler::*` and
/// `aura_runtime::memory_observer::*` today; Phase B keeps those
/// paths working by exposing thin re-export modules over the
/// underlying [`aura_engine`] surface.
pub mod scheduler {
    pub use aura_engine::scheduler::{
        AgentIdentity, AgentIdentityRegistry, ProcessingClaim, Scheduler, SchedulerError,
    };
}

/// Memory-observer re-export module. See [`scheduler`] for the
/// rationale behind keeping the legacy `aura_runtime::*::*` import
/// paths alive after the Phase B engine extraction.
pub mod memory_observer {
    pub use aura_engine::memory_observer::{turn_summary_from_result, MemoryTurnObserver};
}

/// Phase 10 carve-out 1: surface-layer entry point for the
/// `aura-node` binary. The root `main.rs` is reduced to a thin
/// shim that installs tracing + calls this function.
///
/// Installs the Phase D Ctrl+C / Ctrl+Break belt-and-suspenders
/// signal handlers, builds the [`NodeConfig`] from environment
/// variables, and drives [`Node::run`] to its conclusion. Always
/// emits an "exiting cleanly" log line on success so log tails
/// never see a silent process death.
///
/// Lives in `aura-runtime` (rather than `aura-surface-cli`, which
/// the original Phase 10 spec named) because moving it to
/// `aura-surface-cli` would create a dependency cycle:
/// `aura-runtime` owns the `aura-node` binary and would need to
/// depend on `aura-surface-cli` to call into the function, while
/// `aura-surface-cli` already depends on `aura-runtime` for the
/// headless-mode wiring. The `aura_surface_cli::run_node`
/// documented path is preserved through a `pub use` re-export at
/// the surface-cli crate root.
///
/// # Errors
///
/// Surfaces any error from [`Node::run`]. The root binary's
/// `main` bubbles it to the process exit code.
pub async fn run_node() -> anyhow::Result<()> {
    // Phase D (harness v2.2): top-level signal handler so external
    // Ctrl+C produces a deterministic log line + exit code 130
    // instead of the mysterious `0xFFFFFFFF` (= -1 unsigned) that
    // previously appeared with zero panic/abort/unwind diagnostics —
    // indistinguishable from a real crash. `Node::run` already
    // installs its own `with_graceful_shutdown(shutdown_signal())`
    // on the axum server (see `node::shutdown_signal`), which drains
    // in-flight HTTP requests on the first Ctrl+C; this handler is
    // the belt-and-suspenders hard deadline: if axum has not drained
    // within 2s, we exit(130) anyway so the process never silently
    // hangs and the operator always sees an exit cause in the log.
    //
    // `tokio::signal::ctrl_c` on Windows wraps `SetConsoleCtrlHandler`
    // for CTRL_C_EVENT; no platform `#[cfg]` is needed for the basic
    // case. The Windows-only Ctrl+Break branch below covers
    // CTRL_BREAK_EVENT for parity with the axum shutdown signal.
    //
    // Future improvement: thread a single top-level
    // `CancellationToken` through `Node::run` -> `RouterState` ->
    // per-session generation tokens so this handler can `.cancel()`
    // active LLM requests before the 2s timeout, instead of the per-
    // session tokens that live inside `session::ws_handler` /
    // `session::generation` today. Not in scope for Phase D — the
    // hard exit alone fixes the diagnosability problem.
    tokio::spawn(async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                tracing::warn!("received Ctrl+C; initiating graceful shutdown");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                tracing::warn!("graceful shutdown timeout reached; exiting with code 130");
                std::process::exit(130);
            }
            Err(err) => {
                tracing::error!(?err, "failed to install Ctrl+C handler");
            }
        }
    });

    #[cfg(windows)]
    {
        match tokio::signal::windows::ctrl_break() {
            Ok(mut stream) => {
                tokio::spawn(async move {
                    if stream.recv().await.is_some() {
                        tracing::warn!("received Ctrl+Break; initiating graceful shutdown");
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        tracing::warn!("graceful shutdown timeout reached; exiting with code 130");
                        std::process::exit(130);
                    }
                });
            }
            Err(err) => {
                tracing::error!(?err, "failed to install Ctrl+Break handler");
            }
        }
    }

    let config = NodeConfig::from_env();
    let result = Node::new(config).run().await;

    // Phase D: always emit a clean-exit line so log tails show a
    // cause for the process going away — either the Ctrl+C warning
    // above or this info line. Never silence.
    tracing::info!("aura-node exiting cleanly");

    result
}

pub use aura_protocol::{
    AgentCapabilities, AgentIdentity, AgentPersona, ApprovalResponse, AssistantMessageEnd,
    AssistantMessageStart, ConversationMessage, ErrorMsg, FileOp, FilesChanged, InboundMessage,
    InstalledTool, ModelSelection, OutboundMessage, ProjectContext, RuntimeRequest,
    RuntimeRequestType, RuntimeRunResponse, SessionModelOverrides, SessionReady, SessionUsage,
    TextDelta, ThinkingDelta, ToolApprovalDecision, ToolApprovalPrompt, ToolApprovalRemember,
    ToolApprovalResponse, ToolAuth as ProtocolToolAuth, ToolInfo, ToolResultMsg, ToolUseStart,
    UserMessage, WorkspaceLocation,
};

#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::gateway::{create_router, RouterState, RouterStateConfig};
    pub use aura_engine::scheduler::Scheduler;
}

/// Top-level error type for the aura-runtime crate.
#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    /// Server bind or runtime error.
    #[error("server error: {0}")]
    Server(#[from] std::io::Error),

    /// Storage layer failure.
    #[error("store error: {0}")]
    Store(#[from] anyhow::Error),

    /// Address parse failure.
    #[error("invalid bind address: {0}")]
    InvalidAddress(#[from] std::net::AddrParseError),
}
