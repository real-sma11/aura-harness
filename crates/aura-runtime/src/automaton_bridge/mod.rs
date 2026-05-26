//! Bridge between `AutomatonController` (defined in `aura-tools`) and the
//! concrete `AutomatonRuntime` + automaton types (from `aura-automaton`).
//!
//! This module lives in `aura-runtime` because it depends on both crates.
//! It handles: JWT injection, tool executor wiring, event broadcasting,
//! and non-blocking task execution.
//!
//! Automaton bridge wires automaton-runtime surfaces (dev-loop, task-run)
//! into per-agent kernels. Domain mutations performed by automaton
//! orchestration code route through [`KernelDomainGateway`] so every
//! `create_spec` / `transition_task` / `save_message` produces a
//! `System` `DomainMutation` pair in the record log (Invariants §2 / §8).
//!
//! ## Module layout
//!
//! - [`event_channel`] — `EventChannel` + `EventSubscription`, the
//!   replay-history-aware broadcast wrapper used so late WS subscribers
//!   to fast-terminating automatons never miss the terminal event.
//! - [`build`] — `prepare_installed_tools` + `build_kernel`, the
//!   per-agent `Kernel` factory used by both lifecycle entry-points.
//! - [`dispatch`] — `start_dev_loop_with_capabilities` and
//!   `run_task_with_capabilities`, the public entry-points
//!   `AutomatonController::start_dev_loop` / `run_task` delegate to.
//! - `mod.rs` (this file) — `AutomatonBridge` struct, simple
//!   bookkeeping (`new`, `with_scheduler`, `subscribe_events`,
//!   `pause_by_id`, `stop_by_id`, `record_lifecycle_event`, …) plus
//!   the `AutomatonController` trait impl that fans out to
//!   `dispatch::*`.

mod build;
mod dispatch;
mod event_channel;

#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tracing::{info, warn};

use aura_agent::agent_runner::AgentRunnerConfig;
use aura_automaton::{AutomatonHandle, AutomatonRuntime};
use aura_core::{AgentId, SystemKind, Transaction, TransactionType};
use aura_reasoner::ModelProvider;
use aura_store::Store;
use aura_tools::automaton_tools::AutomatonController;
use aura_tools::catalog::ToolCatalog;
use aura_tools::domain_tools::DomainApi;
use aura_tools::ToolConfig;

use crate::jwt_domain::JwtDomainApi;
use crate::scheduler::Scheduler;

pub(crate) use event_channel::EventChannel;
pub use event_channel::EventSubscription;

/// Bookkeeping for a running automaton so stop/pause paths can emit
/// `System::AutomatonLifecycle` entries on the correct agent log
/// without rebuilding the per-agent kernel.
pub(super) struct ProjectHandle {
    pub(super) automaton_id: String,
    pub(super) agent_id: AgentId,
    pub(super) handle: AutomatonHandle,
}

/// Concrete [`AutomatonController`] wired to the real runtime.
pub struct AutomatonBridge {
    runtime: Arc<AutomatonRuntime>,
    // TODO(phase2-followup): Invariant §10 — bind to `Arc<dyn ReadStore>`
    // once `Kernel::new` accepts a read-only store + write hook. The
    // bridge never calls `append_entry_*` itself; it only passes the
    // handle through to `build_kernel` → `Kernel::new`.
    store: Arc<dyn Store>,
    domain: Arc<dyn DomainApi>,
    provider: Arc<dyn ModelProvider + Send + Sync>,
    catalog: Arc<ToolCatalog>,
    tool_config: ToolConfig,
    /// project_id -> tracked (automaton_id, agent_id, handle) tuple.
    ///
    /// The `agent_id` component is carried so lifecycle stop events
    /// recorded by the REST-friendly stop paths can scope the
    /// `System::AutomatonLifecycle` transaction to the same agent log
    /// the corresponding start event landed on (Invariant §2 / §8).
    project_handles: Arc<DashMap<String, ProjectHandle>>,
    /// automaton_id -> replay-aware event channel. See
    /// [`EventChannel`] for why this wraps the broadcast rather than
    /// using one directly.
    event_channels: Arc<DashMap<String, Arc<EventChannel>>>,
    /// Scheduler used to drain the per-agent inbox after a lifecycle
    /// `System` transaction is enqueued. Optional so test harnesses can
    /// construct a bridge without a live scheduler; production wiring
    /// always sets this via [`AutomatonBridge::with_scheduler`].
    scheduler: Option<Arc<Scheduler>>,
}

impl AutomatonBridge {
    pub fn new(
        runtime: Arc<AutomatonRuntime>,
        store: Arc<dyn Store>,
        domain: Arc<dyn DomainApi>,
        provider: Arc<dyn ModelProvider + Send + Sync>,
        catalog: Arc<ToolCatalog>,
        tool_config: ToolConfig,
    ) -> Self {
        Self {
            runtime,
            store,
            domain,
            provider,
            catalog,
            tool_config,
            project_handles: Arc::new(DashMap::new()),
            event_channels: Arc::new(DashMap::new()),
            scheduler: None,
        }
    }

    /// Attach the scheduler used to drain the lifecycle inbox.
    ///
    /// After [`record_lifecycle_event`](Self::record_lifecycle_event)
    /// enqueues a `System::AutomatonLifecycle` transaction, the bridge
    /// immediately requests a scheduling tick for that agent so the
    /// entry is promoted into the record log instead of sitting in the
    /// inbox until the next unrelated wakeup.
    #[must_use]
    pub fn with_scheduler(mut self, scheduler: Arc<Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    /// Subscribe to events for a running automaton.
    ///
    /// Returns an [`EventSubscription`] snapshot that combines the
    /// replay history (events already emitted before this call) with
    /// a live receiver (events emitted from now on). See
    /// [`EventChannel`] for the motivating race: fast-terminating
    /// automatons can finish emitting every event before the first
    /// WebSocket client finishes its handshake, so a bare
    /// `broadcast::Receiver` routinely observed "stream closed with
    /// no terminal event".
    pub fn subscribe_events(&self, automaton_id: &str) -> Option<EventSubscription> {
        self.event_channels.get(automaton_id).map(|entry| {
            let ch = entry.value();
            let history = ch.history.lock().clone();
            EventSubscription {
                history,
                live: ch.broadcast.subscribe(),
                already_done: ch.done.load(Ordering::Acquire),
            }
        })
    }

    /// Wrap domain API with JWT injection when an auth token is available.
    pub(super) fn domain_with_jwt(&self, auth_token: Option<&str>) -> Arc<dyn DomainApi> {
        match auth_token {
            Some(token) if !token.is_empty() => {
                Arc::new(JwtDomainApi::new(self.domain.clone(), token.to_string()))
            }
            _ => self.domain.clone(),
        }
    }

    /// Publish the dev-loop / task-run agent's identity into the
    /// shared [`crate::scheduler::AgentIdentityRegistry`] so the
    /// post-`schedule_agent` worker path (the lifecycle-event nudge
    /// below, plus any tool-permission update fan-out) builds the
    /// per-turn `AgentLoopConfig` with the correct model and
    /// `X-Aura-*` envelope. Without this registration the worker
    /// path strips identity and `aura-router` returns
    /// `429 RATE_LIMITED`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn register_automaton_identity(
        &self,
        agent_id: AgentId,
        model: &str,
        auth_token: Option<&str>,
        aura_org_id: Option<&str>,
        aura_session_id: Option<&str>,
        aura_agent_id: Option<&str>,
        aura_project_id: Option<&str>,
        request_kind: aura_reasoner::ModelRequestKind,
    ) {
        let Some(scheduler) = self.scheduler.as_ref() else {
            return;
        };
        let identity = crate::scheduler::AgentIdentity {
            model: model.to_string(),
            aura_org_id: aura_org_id.map(String::from),
            aura_session_id: aura_session_id.map(String::from),
            aura_agent_id: aura_agent_id.map(String::from),
            aura_project_id: aura_project_id.map(String::from),
            // Dev-loop / task-run paths run their automaton-driven
            // prompt assembly inside `aura-automaton`. The
            // post-completion fan-out scheduling that lands on the
            // worker carries no follow-up user prompt, so an empty
            // system prompt is the right default — the registry is
            // there for envelope identity, not prompt content.
            system_prompt: String::new(),
            prompt_cache_key: aura_project_id.map(|pid| format!("devloop:{pid}")),
            prompt_cache_retention: None,
            request_kind,
            max_tokens: 16384,
            max_context_tokens: crate::session::context_window_for_model(model) as usize,
            auth_token: auth_token.map(String::from),
        };
        scheduler.identity_registry().register(agent_id, identity);
    }

    /// Record an automaton lifecycle event as a System transaction.
    ///
    /// Enqueues a `System::AutomatonLifecycle` transaction on the
    /// agent's inbox and immediately nudges the scheduler so the entry
    /// is promoted into the record log without waiting for an unrelated
    /// wakeup. Scheduler failures are logged but never propagated —
    /// this is a lifecycle side-effect, not the main operation (§2, §8).
    pub(crate) async fn record_lifecycle_event(
        &self,
        agent_id: AgentId,
        automaton_id: &str,
        event: &str,
    ) {
        let payload = serde_json::json!({
            "system_kind": SystemKind::AutomatonLifecycle,
            "automaton_id": automaton_id,
            "event": event,
        });
        let Ok(payload_bytes) = serde_json::to_vec(&payload) else {
            warn!("Failed to serialize lifecycle event payload");
            return;
        };
        let tx = Transaction::new_chained(agent_id, TransactionType::System, payload_bytes, None);
        if let Err(e) = self.store.enqueue_tx(&tx) {
            warn!(error = %e, "Failed to record automaton lifecycle event");
            return;
        }
        // §2 requires that the System transaction eventually appears in
        // the record log. The scheduler drains the inbox through the
        // kernel's single-writer path; awaiting here means the record
        // entry is committed before the caller observes the lifecycle
        // write. Scheduler errors are logged but never propagated — a
        // lifecycle side-effect must not mask the underlying
        // start/stop operation.
        if let Some(scheduler) = self.scheduler.as_ref() {
            if let Err(e) = scheduler.schedule_agent(agent_id).await {
                warn!(
                    agent_id = %agent_id,
                    error = %e,
                    "Scheduler tick after lifecycle event failed"
                );
            }
        }
    }

    pub(super) fn build_runner_config(
        &self,
        model: &str,
        auth_token: Option<&str>,
        aura_org_id: Option<&str>,
        aura_session_id: Option<&str>,
        aura_agent_id: Option<&str>,
        aura_project_id: Option<&str>,
    ) -> AgentRunnerConfig {
        // Dev-loop / task-run paths must carry an explicit model end
        // to end. The pre-fix code took `Option<&str>` and silently
        // fell back to `aura_agent::DEFAULT_MODEL` (`claude-opus-4-6`)
        // — the regression that shipped the wrong upstream model when
        // the user had selected `claude-opus-4-7` from the chat
        // surface. The caller is now responsible for plumbing the
        // user-selected model through; `prepare_automaton_run`
        // already returns an `InvalidConfig` error when it isn't
        // present in the start request.
        let mut config = AgentRunnerConfig::for_agent(model);
        config.max_context_tokens = crate::session::context_window_for_model(model);
        config.auth_token = auth_token.map(String::from);
        // Forward all four router/billing identifiers from the
        // `POST /automaton/start` payload. These flow into
        // `AgentLoopConfig` (see `configure_loop_config`) and then
        // onto every `ModelRequest`, where the Anthropic provider
        // stamps them as `X-Aura-Org-Id` / `X-Aura-Session-Id` /
        // `X-Aura-Agent-Id` / `X-Aura-Project-Id` — matching the
        // headers that interactive chat already sends. Missing
        // `X-Aura-Agent-Id` / `X-Aura-Project-Id` on the dev-loop /
        // task-run path was the WAF trigger: `aura-router`'s
        // Cloudflare rules score requests partly on whether they
        // carry a full aura-os identity envelope, and a stripped
        // envelope made eval bursts read as unsanctioned API
        // traffic and pick up the managed challenge (HTTP 403 +
        // HTML body) that interactive chat from the same account
        // never saw.
        config.aura_org_id = aura_org_id.map(String::from);
        config.aura_session_id = aura_session_id.map(String::from);
        config.aura_agent_id = aura_agent_id.map(String::from);
        config.aura_project_id = aura_project_id.map(String::from);
        // Stable per-project key so OpenAI-family routing buckets all
        // dev-loop / task-run invocations for the same project onto the
        // same prompt cache. Anthropic caching does not need this — the
        // provider's ephemeral `cache_control` breakpoints in
        // `aura_reasoner::anthropic::convert` handle prefix reuse based
        // on byte-identical system/tools/last-user-block content.
        config.prompt_cache_key = aura_project_id.map(|pid| format!("devloop:{pid}"));
        config
    }

    // ------------------------------------------------------------------
    // Direct REST-friendly methods (by automaton_id, not project_id)
    // ------------------------------------------------------------------

    /// Pause an automaton by its ID.
    ///
    /// Mirrors [`Self::stop_by_id`]'s audit trail by recording a
    /// `System::AutomatonLifecycle { event: "pause_dev_loop" }`
    /// transaction on the owning agent's log. Without this entry
    /// operators inspecting the record log can see start / stop
    /// transitions but not the intentional pauses between them,
    /// making "the run halted at 19:27 — was that a pause or a
    /// stop?" impossible to answer from the audit trail alone.
    pub async fn pause_by_id(&self, automaton_id: &str) -> Result<(), String> {
        let mut target: Option<AgentId> = None;
        for entry in self.project_handles.iter() {
            let tracked = entry.value();
            if tracked.automaton_id == automaton_id {
                if tracked.handle.is_finished() {
                    return Err("Automaton has already finished".into());
                }
                tracked.handle.pause();
                target = Some(tracked.agent_id);
                break;
            }
        }
        if let Some(agent_id) = target {
            self.record_lifecycle_event(agent_id, automaton_id, "pause_dev_loop")
                .await;
            info!(automaton_id, "Automaton paused via REST");
            return Ok(());
        }
        Err(format!("Automaton {automaton_id} not found"))
    }

    /// Stop an automaton by its ID.
    pub async fn stop_by_id(&self, automaton_id: &str) -> Result<(), String> {
        let mut target: Option<(String, AgentId)> = None;
        for entry in self.project_handles.iter() {
            let tracked = entry.value();
            if tracked.automaton_id == automaton_id {
                if tracked.handle.is_finished() {
                    let project_id = entry.key().clone();
                    drop(entry);
                    self.project_handles.remove(&project_id);
                    return Err("Automaton has already finished".into());
                }
                tracked.handle.stop();
                target = Some((entry.key().clone(), tracked.agent_id));
                break;
            }
        }
        if let Some((project_id, agent_id)) = target {
            self.project_handles.remove(&project_id);
            self.record_lifecycle_event(agent_id, automaton_id, "stop_dev_loop")
                .await;
            info!(automaton_id, "Automaton stopped via REST");
            return Ok(());
        }
        // Also try the runtime directly (for task runs not in project_handles).
        self.runtime.stop(automaton_id).map_err(|e| e.to_string())
    }

    /// Get the status of an automaton by its ID.
    pub fn get_status(&self, automaton_id: &str) -> Option<aura_automaton::AutomatonInfo> {
        self.runtime.get_info(automaton_id)
    }

    /// List all running automatons.
    pub fn list_automatons(&self) -> Vec<aura_automaton::AutomatonInfo> {
        self.runtime.list()
    }
}

#[async_trait]
impl AutomatonController for AutomatonBridge {
    async fn start_dev_loop(
        &self,
        project_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
    ) -> Result<String, String> {
        self.start_dev_loop_with_capabilities(
            project_id,
            workspace_root,
            auth_token,
            model,
            git_repo_url,
            git_branch,
            None,
            None,
            aura_core::AgentPermissions::full_access(),
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
    }

    async fn pause_dev_loop(&self, project_id: &str) -> Result<(), String> {
        let (automaton_id, agent_id) = {
            let entry = self
                .project_handles
                .get(project_id)
                .ok_or_else(|| format!("No running dev loop for project {project_id}"))?;
            let tracked = entry.value();
            if tracked.handle.is_finished() {
                return Err("Dev loop has already finished".into());
            }
            tracked.handle.pause();
            (tracked.automaton_id.clone(), tracked.agent_id)
        };
        // Mirror `stop_dev_loop`'s audit trail. See `pause_by_id` for
        // why the `System::AutomatonLifecycle` write matters.
        self.record_lifecycle_event(agent_id, &automaton_id, "pause_dev_loop")
            .await;
        info!(project_id, automaton_id = %automaton_id, "Dev loop paused");
        Ok(())
    }

    async fn stop_dev_loop(&self, project_id: &str) -> Result<(), String> {
        let (automaton_id, agent_id) = {
            let entry = self
                .project_handles
                .get(project_id)
                .ok_or_else(|| format!("No running dev loop for project {project_id}"))?;
            let tracked = entry.value();
            if tracked.handle.is_finished() {
                let project_id_owned = project_id.to_string();
                drop(entry);
                self.project_handles.remove(&project_id_owned);
                return Err("Dev loop has already finished".into());
            }
            tracked.handle.stop();
            (tracked.automaton_id.clone(), tracked.agent_id)
        };
        self.project_handles.remove(project_id);
        self.record_lifecycle_event(agent_id, &automaton_id, "stop_dev_loop")
            .await;
        info!(project_id, automaton_id = %automaton_id, "Dev loop stopped");
        Ok(())
    }

    async fn run_task(
        &self,
        project_id: &str,
        task_id: &str,
        workspace_root: Option<PathBuf>,
        auth_token: Option<String>,
        model: Option<String>,
        git_repo_url: Option<String>,
        git_branch: Option<String>,
    ) -> Result<String, String> {
        self.run_task_with_capabilities(
            project_id,
            task_id,
            workspace_root,
            auth_token,
            model,
            git_repo_url,
            git_branch,
            None,
            None,
            aura_core::AgentPermissions::full_access(),
            None,
            Vec::new(),
            None,
            None,
            None,
            None,
            Vec::new(),
            None,
        )
        .await
    }
}
