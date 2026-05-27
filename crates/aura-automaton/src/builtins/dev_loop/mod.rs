//! Dev-loop automaton — runs project tasks in order.
//!
//! Intentionally minimal: fetch all tasks on first tick, drop the ones
//! already marked `done`, sort the rest by `order`, then execute one
//! per tick through [`aura_agent::agent_runner::AgentRunner`]. Status
//! transitions are best-effort writes to the domain API. No retries,
//! no dependency graph, no DoD aggregates, no commit gates, no
//! preflight — those layers belong in higher-level orchestration.
//!
//! `mod.rs` owns the [`DevLoopAutomaton`] façade. The Automaton trait
//! impl and per-task execution live in [`tick`]. The parsed
//! `DevLoopConfig` lives in
//! [`crate::builtins::common::config::DevLoopConfig`] and is stashed
//! on the automaton via an `OnceLock<Arc<DevLoopConfig>>` so per-tick
//! code reads the typed struct instead of reparsing the JSON. The
//! advisory `forward_event` translator and the task-execution / task-
//! finalize / aux-LLM helpers all live in
//! [`crate::builtins::common`].

use std::sync::{Arc, OnceLock};

use aura_agent::agent_runner::{AgentRunner, AgentRunnerConfig};
use aura_reasoner::ModelProvider;
use aura_tools::catalog::ToolCatalog;
use aura_tools::domain_tools::DomainApi;

use crate::builtins::common::config::SharedDevLoopConfig;

mod tick;

#[cfg(test)]
mod tests;

pub struct DevLoopAutomaton {
    pub(crate) domain: Arc<dyn DomainApi>,
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) runner: AgentRunner,
    pub(crate) catalog: Arc<ToolCatalog>,
    pub(crate) tool_executor: Option<Arc<dyn aura_agent::types::AgentToolExecutor>>,
    /// Typed start-request config, parsed once in
    /// [`Automaton::on_install`](crate::runtime::Automaton::on_install)
    /// and read from every subsequent tick. Pre-Phase-6 the dev-loop
    /// reparsed the JSON on every tick (and re-allocated the
    /// `AgentIdentityEnvelope` buffers) — the `OnceLock` here keeps
    /// the install-side hard-fail (`InvalidConfig`) close to the
    /// operator-facing start-request boundary while letting the
    /// `Automaton` trait remain `&self` (no interior mutability cost
    /// beyond the read-side `Arc::clone`).
    pub(crate) parsed_config: OnceLock<SharedDevLoopConfig>,
}

impl DevLoopAutomaton {
    /// Construct a dev-loop automaton bound to a kernel-mediated model
    /// provider.
    ///
    /// The `RecordingModelProvider` bound (sealed in `aura-agent`,
    /// Invariant §1 / §3) means external crates can satisfy this only
    /// by passing an `Arc<aura_agent::KernelModelGateway>`, so a raw
    /// HTTP provider can never reach the dev loop without going through
    /// `Kernel::reason_streaming` first.
    pub fn new<P>(
        domain: Arc<dyn DomainApi>,
        provider: Arc<P>,
        config: AgentRunnerConfig,
        catalog: Arc<ToolCatalog>,
    ) -> Self
    where
        P: aura_agent::RecordingModelProvider + Send + Sync + 'static,
    {
        let provider: Arc<dyn ModelProvider> = provider;
        Self {
            domain,
            provider,
            runner: AgentRunner::new(config),
            catalog,
            tool_executor: None,
            parsed_config: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn with_tool_executor(
        mut self,
        executor: Arc<dyn aura_agent::types::AgentToolExecutor>,
    ) -> Self {
        self.tool_executor = Some(executor);
        self
    }
}
