//! Shared building blocks for the automaton builtins.
//!
//! `dev_loop`, `task_run`, `chat`, `spec_gen`, and `task_refinement`
//! all converge on the same few orchestration patterns:
//!
//! - **forwarding** `aura_agent::AgentLoopEvent`s onto `AutomatonEvent`s
//!   ([`forward_event`]).
//! - **parsing** the start-request JSON into a typed configuration
//!   ([`config`]).
//! - **running** a tracked agent task (`AgenticTaskParams` build,
//!   forwarder spawn, `execute_task_tracked` handoff) once per tick
//!   ([`task_execution`]).
//! - **finalizing** the outcome — success / failure / cancel
//!   transitions plus the rollback that mirrors the dev-loop's
//!   `record_task_cancelled` path ([`finalize`]).
//! - **issuing** the auxiliary single-shot LLM call used by
//!   spec-generation and task-refinement ([`model_call`]).
//!
//! Each file in this module has a single responsibility so the
//! per-automaton dispatch (`dev_loop/tick.rs`, `task_run.rs`,
//! `spec_gen.rs`, etc.) reads as a thin policy layer on top of the
//! shared mechanics here.

pub(crate) mod config;
pub(crate) mod finalize;
pub(crate) mod forward_event;
pub(crate) mod model_call;
pub(crate) mod task_execution;

pub(crate) use config::{AgentIdentityEnvelope, DevLoopConfig};
pub(crate) use finalize::{finalize_task_outcome, TaskOutcome};
pub(crate) use forward_event::spawn_agent_event_forwarder;
pub(crate) use model_call::{run_auxiliary_model_call, AuxiliaryModelCall};
pub(crate) use task_execution::{run_tracked_task, TaskExecutionRequest};

// The `forward_agent_event` / `ForwardOutcome` surface is consumed
// only by `crate::builtins::dev_loop::tests`. Re-export via the
// `common::` path so the test imports stay shallow even though
// production code only needs `spawn_agent_event_forwarder`.
#[cfg(test)]
pub(crate) use forward_event::{forward_agent_event, ForwardOutcome};
