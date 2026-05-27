//! [`AgentLoop`] driver and its public `run*` entry points.
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split. The `LoopState` mutation surface lives in [`super::state`];
//! the stop-reason dispatch + summary-compaction tail lives in
//! [`super::stop_reason`]; this file owns the `run_inner` /
//! `retry_after_context_overflow` orchestration and the synchronous
//! cancellation probe.

use std::sync::Arc;

use aura_reasoner::{Message, ModelProvider, ToolDefinition};
use chrono::Utc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentLoopEvent, DebugEvent};
use crate::types::{AgentLoopResult, AgentToolExecutor};

use super::config::AgentLoopConfig;
use super::{task, AgentLoop};

impl AgentLoop {
    /// Create a new agent loop with the given configuration.
    #[must_use]
    pub const fn new(config: AgentLoopConfig) -> Self {
        Self { config }
    }

    /// Update the auth token for subsequent model requests.
    pub fn set_auth_token(&mut self, token: Option<String>) {
        self.config.auth_token = token;
    }

    /// Get a mutable reference to the config for external injection.
    pub fn config_mut(&mut self) -> &mut AgentLoopConfig {
        &mut self.config
    }

    /// Run the agent loop with the given provider, executor, and initial messages.
    ///
    /// Backward-compatible entry point that delegates to
    /// [`run_with_events`](Self::run_with_events) with no event channel
    /// or cancellation token.
    ///
    /// # Errors
    ///
    /// Returns error if a model call or tool execution fails fatally.
    pub async fn run(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        self.run_with_events(provider, executor, messages, tools, None, None)
            .await
    }

    /// Run the agent loop with streaming events and cancellation support.
    ///
    /// When `event_tx` is `Some`, model calls use streaming and emit
    /// real-time [`AgentLoopEvent`]s through the channel. When `None`, the
    /// loop uses non-streaming `provider.complete()`.
    ///
    /// When `cancellation_token` is `Some`, the loop checks for cancellation
    /// at the start of each iteration and during streaming.
    ///
    /// A per-run tool cache avoids re-executing read-only tools with identical
    /// arguments. The cache is invalidated when any write tool succeeds.
    ///
    /// # Errors
    ///
    /// Returns error if a model call or tool execution fails fatally.
    pub async fn run_with_events(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        event_tx: Option<Sender<AgentLoopEvent>>,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        self.run_with_session(
            provider,
            executor,
            messages,
            tools,
            event_tx,
            cancellation_token,
            None,
        )
        .await
    }

    /// Run the agent loop with an optional session-scoped
    /// [`AgentRunnerHandle`](crate::AgentRunnerHandle) for mid-task
    /// user steering (Layer E.2).
    ///
    /// When `handle` is `Some`, the task shell loops on the wrapped
    /// queue's `has_pending()` flag after each turn so that user
    /// inputs delivered via
    /// [`AgentRunnerHandle::send_user_input`](crate::AgentRunnerHandle::send_user_input)
    /// keep the agent responsive without aborting the conversation.
    /// The handle is taken by reference because the caller typically
    /// keeps a long-lived clone for the UI / RPC thread that issues
    /// the steering inputs. When `None`, behaviour collapses to
    /// [`Self::run_with_events`] (single-turn-per-task semantic from
    /// E.1).
    ///
    /// # Errors
    ///
    /// Returns error if a model call or tool execution fails
    /// fatally, or if the per-task `max_turns_per_task` /
    /// `max_iterations_per_task` ceilings trip.
    // E.2: 8 parameters (one over the default 7 clippy ceiling). The
    // new `handle` is the only addition vs `run_with_events`;
    // bundling provider / executor / messages / tools / event_tx /
    // cancellation into a `RunCtx` struct would force every call
    // site (`agent_runner::execute_chat`, `execute_task_inner`, the
    // mock-driven tests) to introduce a one-shot wrapper just to
    // make space for the new optional arg. Documented per Rule 1.4
    // and tracked for Phase 8 cleanup.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_session(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        event_tx: Option<Sender<AgentLoopEvent>>,
        cancellation_token: Option<CancellationToken>,
        handle: Option<&crate::AgentRunnerHandle>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        // Layer E.4: ALWAYS instantiate an internal [`Session`] so the
        // agent loop has a unified handle to the `InputQueue` +
        // `GoalRuntime` regardless of whether the caller supplied an
        // [`AgentRunnerHandle`]. When `handle` is `Some`, the new
        // session shares the handle's backing queue (and session id);
        // when `None`, we mint a fresh session id + queue paired with
        // either the supplied `cancellation_token` or a freshly
        // created one so in-band cancel + external cancel still share
        // a signal. This is the resolution for E.2's open question:
        // [`crate::agent_runner::AgentRunner::execute_task`] +
        // friends remain the public entry points; everything goes
        // through a session internally.
        let cancellation = cancellation_token.clone().unwrap_or_default();
        let session = match handle {
            Some(h) => crate::session::Session::from_handle(h, cancellation.clone()),
            None => crate::session::Session::new(
                crate::session::SessionId::new_v4(),
                cancellation.clone(),
            ),
        };
        let session = Arc::new(session);
        // Route provider-level `debug.retry` observations back into the
        // `event_tx` channel by installing a task-local observer for
        // the duration of this turn. The observer forwards through the
        // same channel as UI events so downstream consumers see all
        // `debug.*` frames inline with the streaming text.
        let observer: Option<aura_reasoner::RetryObserver> = event_tx.as_ref().map(|tx| {
            let tx = tx.clone();
            Arc::new(move |info: aura_reasoner::RetryInfo| {
                let event = AgentLoopEvent::Debug(DebugEvent::Retry {
                    timestamp: Utc::now(),
                    reason: info.reason,
                    attempt: info.attempt,
                    wait_ms: info.wait_ms,
                    provider: Some(info.provider),
                    model: Some(info.model),
                    task_id: None,
                });
                if let Err(e) = tx.try_send(event) {
                    tracing::warn!("debug.retry channel full or closed: {e}");
                }
            }) as aura_reasoner::RetryObserver
        });

        let fut = self.run_inner(
            provider,
            executor,
            messages,
            tools,
            event_tx,
            cancellation_token,
            session,
        );
        match observer {
            Some(obs) => aura_reasoner::DEBUG_RETRY_OBSERVER.scope(obs, fut).await,
            None => fut.await,
        }
    }

    // E.4: 8 parameters (one over the default 7 clippy ceiling). The
    // new `session` parameter is the unified handle to the
    // [`InputQueue`] + [`GoalRuntime`] for the in-flight session;
    // packing the rest into a struct would force every helper inside
    // this module to learn a new wrapper type. Documented per
    // Rule 1.4.
    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        event_tx: Option<Sender<AgentLoopEvent>>,
        cancellation_token: Option<CancellationToken>,
        session: Arc<crate::session::Session>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        // Layer E.1 + E.2 + E.4: delegate to the nested task → turn →
        // sampling topology. The session carries the input queue and
        // the goal runtime; the latter is consulted by
        // [`super::turn::run_turn_stop_hooks`] to drive the codex-parity
        // continuation logic. See `agent_loop/turn.rs`'s module-level
        // docs for the topology diagram.
        task::run_task(
            self,
            provider,
            executor,
            messages,
            tools,
            event_tx,
            cancellation_token,
            session,
        )
        .await
    }
}

/// Layer E.1 helper retained as a free function (rather than a
/// `Option::is_some_and` call at each site) so that
/// [`super::sampling::run_sampling_request`], [`super::turn::run_turn`],
/// and the pre-E.1 entry points all share one branch-free probe.
pub(crate) fn is_cancelled(token: Option<&CancellationToken>) -> bool {
    token.is_some_and(CancellationToken::is_cancelled)
}
