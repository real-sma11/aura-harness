//! [`ChildRunner`] trait — pluggable child-execution surface.
//!
//! Decouples [`crate::FleetSpawner`] from the runtime-side scheduler
//! / identity registry / agent-loop wiring that today lives in
//! `aura-runtime`. Fleet-spawn invokes the runner once the lease,
//! quota, audit write, and registry slot are in place.
//!
//! The trait deliberately speaks only in [`aura_core_types`],
//! [`aura_core_permissions`], and [`aura_agent_subagent`] types so
//! the fleet layer stays free of upward dependencies on agent /
//! runtime crates.

use async_trait::async_trait;
use aura_agent_subagent::SubagentSpec;
use aura_core_types::{AgentId, SubagentResult};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

/// Errors a [`ChildRunner`] may surface.
#[derive(Debug, Error)]
pub enum ChildRunError {
    /// Runner-internal failure (scheduler error, identity
    /// registration failure, kernel error). String-typed so the
    /// concrete runtime can map any error variant in.
    #[error("child runner error: {0}")]
    Internal(String),
}

/// Bundle of data the [`ChildRunner`] receives per call. Wrapping
/// the args in a struct keeps the trait signature stable as the
/// override surface grows.
///
/// Phase 7b retires the Phase 7a `TaskCompatContext` shim: every
/// field the legacy task path needed (`subagent_type`,
/// `system_prompt_addendum`, `parent_tool_permissions`,
/// `user_tool_defaults`, parent lineage / model snapshot) is now
/// modelled directly on [`SubagentSpec`] / [`ChildRunContext`].
#[derive(Debug)]
pub struct ChildRunContext {
    /// Derived spec from `aura-agent-subagent`. Carries the full
    /// resolved + narrowed surface the runner needs to bring up a
    /// child loop.
    pub spec: SubagentSpec,
    /// Initial prompt for the child.
    pub prompt: String,
    /// Originating user id for audit attribution.
    pub originating_user_id: Option<String>,
    /// Parent agent id forwarded so the runner can register the
    /// child identity in the scheduler.
    pub parent_agent_id: AgentId,
    /// Parent's `parent_chain` snapshot — propagated so the child
    /// inherits the audit attribution chain.
    pub parent_chain: Vec<AgentId>,
    /// Cancellation token the runner MUST poll between safe yield
    /// points. When the token fires the runner is expected to
    /// short-circuit with a [`SubagentResult`] whose
    /// [`aura_core_types::SubagentExit::Cancelled`] tag is set.
    pub cancellation: CancellationToken,
    /// Pre-assigned child agent id. The spawner allocates the id
    /// up-front so the spawn audit record and the
    /// [`crate::SpawnHandle::Detached`] handle carry the same value;
    /// runners that previously called into `KernelSpawnHook` MUST
    /// thread this id back into `ChildAgentSpec::preassigned_agent_id`.
    pub preassigned_agent_id: AgentId,
    /// Optional streaming sink for the child agent loop's
    /// [`AgentLoopEvent`](aura_agent_loop::AgentLoopEvent)s. When
    /// `Some`, the runner is expected to drive the child loop via
    /// `run_with_events` so an external observer (e.g. a WS client
    /// attached to the child's minted run id) sees live text / tool
    /// frames. When `None`, the runner uses the non-streaming path —
    /// preserving the existing blocking-Wait behavior byte-for-byte.
    ///
    /// The fleet layer only forwards this sender; it never constructs
    /// wire-protocol messages from it (that mapping stays in
    /// `aura-runtime`).
    pub event_tx: Option<tokio::sync::mpsc::Sender<aura_agent_loop::AgentLoopEvent>>,
}

/// Run a derived [`SubagentSpec`] to completion and return a
/// [`SubagentResult`] for the parent's tool call to consume.
///
/// Implementations are expected to:
///
/// - Look up the bundled subagent kind (or other registry) the
///   spec references via `ctx.spec.kind` / `ctx.spec.subagent_type`.
/// - Register the child's identity with the scheduler.
/// - Enqueue the child's initial prompt as a transaction.
/// - Run the child agent loop to completion (with the spec's
///   timeout), polling [`ChildRunContext::cancellation`] at every
///   safe yield point.
/// - Translate the agent-loop result into a [`SubagentResult`] with
///   the exact same field semantics as today's
///   `RuntimeSubagentDispatch::dispatch` so the task tool's
///   surface remains byte-for-byte stable.
#[async_trait]
pub trait ChildRunner: Send + Sync {
    /// Run the child loop and return its terminal
    /// [`SubagentResult`].
    ///
    /// # Errors
    ///
    /// Returns [`ChildRunError`] if the runner could not start, the
    /// scheduler errored, or the loop produced no result. A child
    /// timeout / failure is returned INSIDE a successful
    /// [`SubagentResult`] (`exit: SubagentExit::Timeout` /
    /// `Failed`) — only infrastructure failures bubble up as an
    /// error.
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError>;
}
