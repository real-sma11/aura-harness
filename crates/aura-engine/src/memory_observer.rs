//! Runtime-side adapter bridging the agent loop's `TurnObserver` contract
//! to the `aura-context-memory` write pipeline.
//!
//! Phase 6c inverted the historical upward edge `aura-context-memory ->
//! aura-agent`. The memory crate now consumes a layer-neutral
//! [`aura_context_memory::TurnSummary`] and exposes no observer trait of its own.
//! This module lives in the engine layer — the natural meeting point of
//! `aura-agent` (for the [`aura_agent::TurnObserver`] trait + the
//! [`aura_agent::AgentLoopResult`] payload) and `aura-context-memory`
//! (for the [`aura_context_memory::MemoryManager`] facade) — and supplies the
//! glue that:
//!
//! 1. Translates an [`aura_agent::AgentLoopResult`] into a
//!    [`aura_context_memory::TurnSummary`] via [`turn_summary_from_result`].
//! 2. Implements [`aura_agent::TurnObserver`] for [`MemoryTurnObserver`]
//!    and delegates to
//!    [`aura_context_memory::MemoryManager::process_result_with_context`].
//!
//! Memory ingestion is intentionally best-effort: a failed write logs a
//! `warn!` and returns, never aborting the in-flight turn (Invariant:
//! conversational correctness is independent of the memory pipeline's
//! liveness).

use std::sync::Arc;

use async_trait::async_trait;
use aura_agent::{AgentLoopResult, TurnObserver};
use aura_context_memory::{MemoryManager, TurnSummary};
use aura_core_types::AgentId;

/// Build a layer-neutral [`TurnSummary`] from an
/// [`AgentLoopResult`].
///
/// Performs a member-by-member copy of the subset of fields the memory
/// pipeline actually consumes. The `messages` vec is cloned because the
/// pipeline derives a `ConversationTurn` from it after the turn has
/// settled — the owning `AgentLoopResult` is no longer accessible by
/// then (the agent-loop task has already returned and dropped it).
#[must_use]
pub fn turn_summary_from_result(result: &AgentLoopResult) -> TurnSummary {
    TurnSummary {
        timed_out: result.timed_out,
        stalled: result.stalled,
        llm_error: result.llm_error.clone(),
        total_text: result.total_text.clone(),
        total_input_tokens: result.total_input_tokens,
        total_output_tokens: result.total_output_tokens,
        iterations: result.iterations,
        messages: result.messages.clone(),
    }
}

/// `TurnObserver` adapter that feeds completed turns into a
/// [`MemoryManager`].
///
/// Constructed via [`MemoryTurnObserver::new`], which returns an
/// `Arc<Self>` so callers can push it directly into
/// `AgentLoopConfig::observers` (the agent loop stores observers as
/// `Arc<dyn TurnObserver>`).
pub struct MemoryTurnObserver {
    manager: Arc<MemoryManager>,
    agent_id: AgentId,
    auth_token: Option<String>,
    active_skills: Vec<String>,
    source_session_id: Option<String>,
}

impl MemoryTurnObserver {
    /// Build an `Arc<Self>` ready to be pushed into
    /// `AgentLoopConfig::observers`.
    ///
    /// - `manager` is the shared per-process [`MemoryManager`].
    /// - `agent_id` is the per-agent memory id (note: distinct from the
    ///   harness agent id when the WS path remaps via
    ///   `Session::memory_agent_id()`).
    /// - `auth_token` is the session JWT forwarded to proxy-mode LLM
    ///   calls inside the refiner; `None` when no proxy is configured.
    /// - `active_skills` are the skill names injected into the system
    ///   prompt for the current turn so the refiner can tag extracted
    ///   procedures with the relevant skill.
    #[must_use]
    pub fn new(
        manager: Arc<MemoryManager>,
        agent_id: AgentId,
        auth_token: Option<String>,
        active_skills: Vec<String>,
        source_session_id: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            manager,
            agent_id,
            auth_token,
            active_skills,
            source_session_id,
        })
    }
}

#[async_trait]
impl TurnObserver for MemoryTurnObserver {
    async fn on_turn_complete(&self, result: &AgentLoopResult) {
        let summary = turn_summary_from_result(result);
        self.manager.process_result_in_background(
            self.agent_id,
            summary,
            self.auth_token.clone(),
            self.active_skills.clone(),
            self.source_session_id.clone(),
        );
    }
}
