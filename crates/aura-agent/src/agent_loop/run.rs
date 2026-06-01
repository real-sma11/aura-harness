//! [`AgentLoop`] driver and its public `run*` entry points.
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split. The `LoopState` mutation surface lives in [`super::state`];
//! the stop-reason dispatch + summary-compaction tail lives in
//! [`super::stop_reason`]; this file owns the `run_inner` /
//! `retry_after_context_overflow` orchestration and the synchronous
//! cancellation probe.
//!
//! Phase 8 collapsed the previous 8-parameter `run_with_session` /
//! `run_inner` signatures into a single [`super::cx::RunCtx`] that
//! threads the per-run service borrows down to [`super::task::run_task`]
//! and beyond.

use std::sync::Arc;
use std::time::Instant;

use aura_plugin_hooks::HookEvent;
use aura_model_reasoner::{Message, ModelProvider, ToolDefinition};
use chrono::Utc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentLoopEvent, DebugEvent};
use crate::types::{AgentLoopResult, AgentToolExecutor};

use super::config::AgentLoopConfig;
use super::cx::{RunCtx, RunOptions};
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
            RunOptions {
                event_tx,
                cancellation_token,
                handle: None,
            },
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
    /// Phase 8 collapsed the previous 7-borrow signature on the
    /// internal `run_inner` helper into a single [`RunCtx`] that
    /// threads through the entire `task → turn → sampling` topology
    /// without a `too_many_arguments` allow.
    ///
    /// # Errors
    ///
    /// Returns error if a model call or tool execution fails
    /// fatally, or if the per-task `max_turns_per_task` /
    /// `max_iterations_per_task` ceilings trip.
    pub async fn run_with_session(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        mut messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        options: RunOptions<'_>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        let RunOptions {
            event_tx,
            cancellation_token,
            handle,
        } = options;
        // Phase 8: fire UserPromptSubmit hook for the most recent
        // user message in the input history. Handlers may return
        // `Replace { new_value }` to mutate the prompt before it
        // enters the model context, or `Block { reason }` to drop
        // the turn entirely. The `is_empty(event)` short-circuit
        // guarantees zero overhead when no plugins are enabled.
        if let Some(host) = self.config.plugin_hooks.as_ref() {
            if !host.is_empty(HookEvent::UserPromptSubmit) {
                if let Some(idx) = messages
                    .iter()
                    .rposition(|m| matches!(m.role, aura_model_reasoner::Role::User))
                {
                    let prompt_text = messages[idx].text_content();
                    let outcome =
                        host.fire_user_prompt_submit(&prompt_text, Utc::now().to_rfc3339());
                    match &outcome.decision {
                        aura_plugin_hooks::HookOutcome::Replace { new_value } => {
                            messages[idx] = aura_model_reasoner::Message::user(new_value);
                        }
                        aura_plugin_hooks::HookOutcome::Block { reason } => {
                            tracing::info!(
                                hook_reason = %reason,
                                "UserPromptSubmit hook blocked prompt; returning empty result"
                            );
                            return Ok(AgentLoopResult {
                                timed_out: false,
                                insufficient_credits: false,
                                stalled: false,
                                llm_error: Some(format!("prompt blocked by plugin hook: {reason}")),
                                total_text: String::new(),
                                total_thinking: String::new(),
                                total_input_tokens: 0,
                                total_output_tokens: 0,
                                total_cache_creation_input_tokens: 0,
                                total_cache_read_input_tokens: 0,
                                estimated_context_tokens: 0,
                                context_breakdown: Default::default(),
                                context_contents: Default::default(),
                                file_changes: Vec::new(),
                                iterations: 0,
                                messages,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        let started_at = Instant::now();
        // ALWAYS instantiate an internal [`Session`] so the agent
        // loop has a unified handle to the `InputQueue` regardless
        // of whether the caller supplied an [`AgentRunnerHandle`].
        // When `handle` is `Some`, the new session shares the
        // handle's backing queue (and session id); when `None`, we
        // mint a fresh session id + queue paired with either the
        // supplied `cancellation_token` or a freshly created one so
        // in-band cancel + external cancel still share a signal.
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
        let observer: Option<aura_model_reasoner::RetryObserver> = event_tx.as_ref().map(|tx| {
            let tx = tx.clone();
            Arc::new(move |info: aura_model_reasoner::RetryInfo| {
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
            }) as aura_model_reasoner::RetryObserver
        });

        let ctx = RunCtx {
            agent: self,
            provider,
            executor,
            event_tx: event_tx.as_ref(),
            cancellation_token: cancellation_token.as_ref(),
            session: session.as_ref(),
        };
        let fut = self.run_inner(&ctx, messages, tools);
        let result = match observer {
            Some(obs) => aura_model_reasoner::DEBUG_RETRY_OBSERVER.scope(obs, fut).await,
            None => fut.await,
        };
        // Phase 8: fire `Stop` when the agent loop completes
        // *cleanly* (i.e. `Ok(_)`). The hook is observer-only —
        // its outcome cannot mutate the result. Errors and
        // cancellations bypass the hook (those are not natural
        // stops). The `is_empty(event)` short-circuit guarantees
        // zero overhead when no plugins are enabled.
        if let Ok(ref agent_result) = result {
            if let Some(host) = self.config.plugin_hooks.as_ref() {
                if !host.is_empty(HookEvent::Stop) {
                    let duration_ms =
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
                    host.fire_stop(
                        u32::try_from(agent_result.iterations).unwrap_or(u32::MAX),
                        agent_result.total_input_tokens,
                        agent_result.total_output_tokens,
                        duration_ms,
                    );
                }
            }
        }
        result
    }

    async fn run_inner(
        &self,
        ctx: &RunCtx<'_>,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        // Layer E.1 + E.2 + E.4: delegate to the nested task → turn →
        // sampling topology. The session carries the input queue and
        // the goal runtime; the latter is consulted by
        // [`super::turn::run_turn_stop_hooks`] to drive the codex-parity
        // continuation logic. See `agent_loop/turn.rs`'s module-level
        // docs for the topology diagram.
        task::run_task(ctx, messages, tools).await
    }
}

/// Layer E.1 helper retained as a free function (rather than a
/// `Option::is_some_and` call at each site) so that
/// [`super::sampling::run_sampling_request`], [`super::turn::run_turn`],
/// and the pre-E.1 entry points all share one branch-free probe.
pub(crate) fn is_cancelled(token: Option<&CancellationToken>) -> bool {
    token.is_some_and(CancellationToken::is_cancelled)
}
