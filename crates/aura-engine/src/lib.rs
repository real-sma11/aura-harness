//! # aura-engine
//!
//! Layer: surface
//!
//! Aura runtime orchestration engine — scheduler, worker, automaton
//! bridge, child runner, memory observer, runtime capabilities, and
//! executor factory.
//!
//! The HTTP/WS gateway in [`aura-runtime`] composes the primitives
//! exposed here and hands them [`aura_protocol::RuntimeRequest`]
//! payloads. The engine never owns inbound HTTP/WS plumbing; the
//! gateway never owns the orchestration loop.
//!
//! ## Surface
//!
//! - [`scheduler`] — per-agent single-writer claim, identity registry,
//!   per-turn `AgentLoopConfig` resolution. Same `Scheduler` API the
//!   gateway used pre-extraction.
//! - [`worker`] — kernel-mediated agent-loop driver. Owns the
//!   `AGENT_LOOP_TIMEOUT` boundary timeout.
//! - [`automaton`] — `AutomatonBridge` that wires `aura-automaton`'s
//!   dev-loop / task-run automatons through per-agent kernels with
//!   recorded `System::AutomatonLifecycle` audit entries.
//! - [`child_runner`] — [`aura_fleet_spawn::ChildRunner`] impl that
//!   drives the scheduler for foreground subagent dispatch. The
//!   fleet-layer dispatcher (`aura-fleet-subagent`) consumes this
//!   via the `Arc<dyn ChildRunner>` trait object so the agent layer
//!   stays free of fleet deps.
//! - [`memory_observer`] — `TurnObserver` adapter feeding completed
//!   turns into [`aura_memory::MemoryManager`].
//! - [`capabilities`] — `record_runtime_capabilities` helper called by
//!   the automaton bridge during dev-loop / task-run bootstrap.
//! - [`executor`] — shared `ToolResolver` / `ExecutorRouter`
//!   construction helpers.
//! - [`jwt_domain`] — `JwtDomainApi` wrapper used by the automaton
//!   bridge to inject the per-run JWT into `DomainApi` calls. Phase C
//!   relocates this to a dedicated `aura-domain-http` crate alongside
//!   the HTTP `DomainApi` impl; for Phase B it travels with the
//!   automaton bridge to keep the engine self-contained.
//! - [`model_context`] — `context_window_for_model` lookup used by the
//!   automaton bridge and the gateway-side session config.

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

pub mod automaton;
pub mod capabilities;
pub mod child_runner;
pub mod executor;
pub mod jwt_domain;
pub mod memory_observer;
pub mod model_context;
pub mod scheduler;
pub mod worker;

pub use automaton::{AutomatonBridge, EventSubscription};
pub use child_runner::RuntimeChildRunner;
pub use memory_observer::{turn_summary_from_result, MemoryTurnObserver};
pub use model_context::context_window_for_model;
pub use scheduler::{AgentIdentity, AgentIdentityRegistry, Scheduler, SchedulerError};
pub use worker::{process_agent_detailed, ProcessedAgent};
