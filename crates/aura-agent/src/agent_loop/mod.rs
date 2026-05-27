//! Main agent loop orchestrator.
//!
//! `AgentLoop` drives the multi-step agentic conversation by calling
//! the model provider in a loop with intelligence: blocking detection,
//! compaction, sanitization, budget management, etc.

mod context;
mod iteration;
mod sampling;
mod search_cache;
mod stream_pump;
mod streaming;
mod task;
mod tool_execution;
#[cfg(test)]
mod tool_execution_tests;
mod tool_pipeline;
mod turn;
mod turn_diff;

pub use task::TaskId;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod cutover_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod pipeline_tests;
#[cfg(test)]
mod shamir_replay_tests;
#[cfg(test)]
mod streaming_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_advanced;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelRequestKind, Role, StopReason,
    ThinkingEffort, ToolChoice, ToolDefinition,
};
use aura_tools::IntentClassifier;
use chrono::Utc;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::budget::{BudgetState, ExplorationState};
use crate::constants::{
    AUTO_BUILD_COOLDOWN, CHARS_PER_TOKEN, MAX_ITERATIONS, THINKING_AUTO_ENABLE_THRESHOLD,
    THINKING_MIN_BUDGET, THINKING_TAPER_AFTER, THINKING_TAPER_FACTOR,
};
use crate::events::{AgentLoopEvent, DebugEvent};
use crate::types::{AgentLoopResult, AgentToolExecutor, BuildBaseline, TurnObserver};

/// Configuration for the agent loop.
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// Maximum iterations (model calls) per turn.
    ///
    /// Defaults to [`crate::constants::MAX_ITERATIONS`], which is
    /// itself derived from [`aura_core::MAX_TURNS`] — the single
    /// source of truth for every "max turns / max iterations" knob.
    /// Termination is also driven by `EndTurn` from the model,
    /// exhaustion of [`Self::credit_budget`], stall detection, or
    /// cooperative cancellation. Callers can still pass `usize::MAX`
    /// explicitly to opt into the unlimited-mode short-circuit in
    /// [`crate::budget::should_stop_for_budget`] and
    /// `aura_agent::agent_loop::context::check_budget_warnings`.
    pub max_iterations: usize,
    /// Maximum tokens per response.
    pub max_tokens: u32,
    /// Initial response-token budget seeded into [`LoopState::thinking`]
    /// at the start of a turn. When `Some`, this overrides the default
    /// of `max_tokens` and lets the runner cap the loop's *starting*
    /// budget below `max_tokens` (e.g. derived from the model card's
    /// `max_thinking_tokens`) while still allowing the on-truncation
    /// restore path to lift back to the full `max_tokens` ceiling.
    /// When `None`, the loop falls back to `max_tokens` (preserving the
    /// pre-Phase-6 behavior).
    pub thinking_budget: Option<u32>,
    /// Streaming timeout per iteration. This is an outer guard around
    /// the provider call; it must be >= the reasoner's reqwest request
    /// timeout (`AURA_MODEL_TIMEOUT_MS`, default 300s) or the agent
    /// loop will fire "Model call timed out" while the provider is
    /// still happily streaming tokens. Aligning the two keeps timeout
    /// responsibility in a single layer (the HTTP client) instead of
    /// producing split-timeout races that look like provider errors.
    pub stream_timeout: Duration,
    /// Credit attribution label.
    pub billing_reason: String,
    /// Loop-level model override.
    pub model_override: Option<String>,
    /// Maximum context tokens for compaction.
    pub max_context_tokens: Option<u64>,
    /// Credit budget (total tokens allowed).
    pub credit_budget: Option<u64>,
    /// Auto-build cooldown in iterations.
    pub auto_build_cooldown: usize,
    /// Thinking budget taper starts after this iteration.
    pub thinking_taper_after: usize,
    /// Factor to reduce thinking budget.
    pub thinking_taper_factor: f64,
    /// Minimum thinking budget after tapering.
    pub thinking_min_budget: u32,
    /// Additional tool definitions beyond core tools.
    pub extra_tools: Vec<ToolDefinition>,
    /// System prompt to use.
    pub system_prompt: String,
    /// Model name.
    pub model: String,
    /// JWT auth token for proxy routing.
    pub auth_token: Option<String>,
    /// Optional upstream provider family hint for managed proxy routing.
    pub upstream_provider_family: Option<String>,
    /// Tool names the user wants prioritized for the current turn.
    /// When present, the model only sees this scoped tool surface for
    /// the whole turn sequence. Tool choice remains `auto` because
    /// Anthropic rejects forced tool use while extended thinking is enabled.
    pub tool_hints: Option<Vec<String>>,
    /// Project ID for X-Aura-Project-Id billing header.
    pub aura_project_id: Option<String>,
    /// Project-agent UUID for X-Aura-Agent-Id billing header.
    pub aura_agent_id: Option<String>,
    /// Storage session UUID for X-Aura-Session-Id billing header.
    pub aura_session_id: Option<String>,
    /// Org UUID for X-Aura-Org-Id billing header.
    pub aura_org_id: Option<String>,
    /// Stable `prompt_cache_key` forwarded to OpenAI-family routing. See
    /// `aura_reasoner::ModelRequest::prompt_cache_key`.
    pub prompt_cache_key: Option<String>,
    /// Retention hint paired with `prompt_cache_key`. Wire values
    /// `"in_memory"` / `"24h"`.
    pub prompt_cache_retention: Option<String>,
    /// Request contract kind used when building provider-bound requests.
    ///
    /// Chat/session callers default to [`ModelRequestKind::Chat`]. Task
    /// automation sets this to [`ModelRequestKind::DevLoopBootstrap`];
    /// after the first iteration the loop automatically sends
    /// [`ModelRequestKind::DevLoopContinuation`] for that flow.
    pub request_kind: ModelRequestKind,
    /// Post-turn observers (e.g. memory ingestion).
    /// Called automatically at the end of every turn inside the loop.
    pub observers: Vec<Arc<dyn TurnObserver>>,
    /// Optional keyword-driven intent classifier used to narrow the
    /// per-turn visible tool set based on the latest user message.
    ///
    /// Ships with an accompanying [`intent_classifier_manifest`] that
    /// maps tool names to their snake-case domain. Tools not present in
    /// the manifest are passed through unchanged, so core filesystem /
    /// shell tools stay visible regardless of classifier state.
    ///
    /// Populated via [`aura_protocol::SessionInit::intent_classifier`]
    /// (see `aura-os-super-agent::harness_handoff`) to let the harness
    /// reproduce the aura-os CEO super-agent's tier-1/tier-2 filtering
    /// without baking the tool manifest into the harness binary.
    ///
    /// [`intent_classifier_manifest`]: Self::intent_classifier_manifest
    pub intent_classifier: Option<Arc<IntentClassifier>>,
    /// `(tool_name, domain)` pairs consumed by [`intent_classifier`].
    ///
    /// Empty when [`intent_classifier`] is `None`.
    ///
    /// [`intent_classifier`]: Self::intent_classifier
    pub intent_classifier_manifest: Vec<(String, String)>,
    /// Character count of the static skills surface for this session,
    /// used by the per-turn context breakdown to estimate the "Skills"
    /// bucket. Computed once at session start by the runtime crate from
    /// the resolved [`aura_protocol::SkillInfo`] list. Defaults to `0`
    /// when the harness is run without skills wired in (e.g. unit
    /// tests, dev loop), in which case the bucket reads as zero.
    pub skills_chars: usize,
    /// Character count of the static subagent registry for this
    /// session, used by the per-turn context breakdown to estimate the
    /// "Subagents" bucket. Computed once at session start by the
    /// runtime crate from the active [`aura_runtime::SubagentRegistry`].
    /// Defaults to `0` for the same reasons as [`Self::skills_chars`].
    pub subagents_chars: usize,
    /// Optional handshake from a wrapping
    /// [`crate::task_executor::TaskToolExecutor`]: when the inner
    /// `Arc<AtomicBool>` flips to `true`, [`LoopState::begin_iteration`]
    /// resets the exploration counter so the implementation phase has
    /// a fresh budget. `None` for non-task callers (e.g. chat).
    pub phase_reset_signal: Option<Arc<AtomicBool>>,
    /// Dev-loop signal: pin reasoning effort to
    /// [`ThinkingEffort::Medium`] across every iteration of the run
    /// (see [`LoopState::compute_thinking_effort`]).
    ///
    /// `configure_loop_config` sets this `true` exclusively for
    /// dev-loop tasks; chat / generic callers leave it `false` and
    /// keep the codex-style `Off → Medium → Low` taper. The field name
    /// is a historical artifact: it used to also arm an iteration-0
    /// `max_tokens` clamp (capping the explore turn below the
    /// Anthropic auto-thinking threshold so the first turn emitted
    /// fast tool calls instead of "Thought for 2m"-bursting). That
    /// clamp was removed in 2026-05 because it contradicted the
    /// pin-to-Medium policy — Medium effort with a 2048-token cap
    /// either rejects the request outright (Claude 3.7 `enabled`
    /// mode) or starves Adaptive thinking of meaningful budget. The
    /// flag now only feeds [`LoopState::compute_thinking_effort`].
    pub disable_thinking_iteration_0: bool,
    /// Layer E.1: hard cap on the number of *turns* one task may run.
    /// A turn is the unit of work between "model starts talking" and
    /// "model goes quiet without follow-up signal"; codex's
    /// `regular.rs` task shell loops on turns until `input_queue` is
    /// empty. E.1 has no `input_queue` yet (lands in E.2), so the
    /// task shell runs exactly one turn per task and this cap is
    /// effectively dormant. Default derives from
    /// [`aura_core::MAX_TURNS`] — the single source of truth for every
    /// "max turns / max iterations" knob. The cap exists to surface a
    /// typed [`AgentError::TurnBudgetExceeded`](crate::AgentError::TurnBudgetExceeded)
    /// instead of letting a runaway `input_queue` push the agent
    /// indefinitely.
    pub max_turns_per_task: u32,
    /// Layer E.1: hard cap on the *total* number of sampling
    /// requests (model round-trips) across every turn of one task.
    /// Companion to [`Self::max_iterations`] (per-turn) and
    /// [`Self::max_turns_per_task`] (turn count); all three default to
    /// [`aura_core::MAX_TURNS`]. Trips surface as
    /// [`AgentError::TurnBudgetExceeded`](crate::AgentError::TurnBudgetExceeded).
    pub max_iterations_per_task: u32,
    /// Layer E.3: per-event boundary timeout for the streaming
    /// sampling pump (Rule 6.2). Each `stream.try_next().await`
    /// is wrapped with `tokio::time::timeout(stream_event_timeout, …)`
    /// so a silent stream surfaces as an
    /// [`AgentError::StreamTimeout`](crate::AgentError::StreamTimeout)
    /// instead of hanging the turn forever. Default `90s` —
    /// comfortably above the typical inter-event gap (Anthropic
    /// ping cadence is 10–15s) but below the parent
    /// `stream_timeout` (`300s`) so the per-event timeout fires
    /// first and the operator sees the more specific error.
    pub stream_event_timeout: Duration,
    /// Layer E.3: per-tool execution timeout enforced inside the
    /// streaming pump's `spawn_tool_call` wrapper (Rule 6.2).
    /// Each in-flight tool future is wrapped with
    /// `tokio::time::timeout(per_tool_timeout, …)`; a hung tool
    /// resolves to a synthetic [`crate::ToolCallResult`] error
    /// instead of poisoning the FIFO drain. Default `300s` matches
    /// the existing reasoner-side `stream_timeout` so long shell /
    /// build commands keep working while a runaway tool is bounded.
    pub per_tool_timeout: Duration,
    /// Layer E.4: switch for the streaming sampling pump
    /// (`agent_loop::stream_pump`). When `true` (default), sampling
    /// drives `provider.complete_response_stream(…)` and overlaps
    /// tool execution at `OutputItemDone` boundaries via
    /// [`futures_util::stream::FuturesOrdered`]. When `false`,
    /// sampling uses the legacy buffered `call_model` +
    /// `dispatch_stop_reason` path.
    ///
    /// # Default flipped to `true` in E.4
    ///
    /// E.3 shipped the pump opt-in because three parity gaps
    /// blocked promotion to the production path: per-delta event
    /// emission (`TextDelta` / `ThinkingDelta` / `ToolInputSnapshot`),
    /// tool-result caching (`split_cached` / `update_cache`), and
    /// pump-triggered auto-build (`run_auto_build`). E.4 wired all
    /// three through the pump (per-`OutputItemDone` block deltas via
    /// the `event_tx` channel, cache consulted at submission time,
    /// auto-build fired in `handle_streamed_tool_use`) so this
    /// flag now defaults to `true`.
    ///
    /// # Remaining caveats
    ///
    /// Sub-block per-token deltas remain on the buffered path — the
    /// pump emits one delta per finished block, which is sufficient
    /// for chat UX continuity but coarser than the per-token feel of
    /// the buffered path.
    pub use_stream_pump: bool,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopConfig")
            .field("max_iterations", &self.max_iterations)
            .field("model", &self.model)
            .field("observers", &self.observers.len())
            .finish_non_exhaustive()
    }
}

impl AgentLoopConfig {
    /// Construct an [`AgentLoopConfig`] for an explicit, caller-supplied
    /// model. **There is no `Default` impl on purpose** — every config
    /// must be born with a real model identifier, otherwise the worker
    /// path silently routes traffic for the wrong model (the
    /// `claude-opus-4-6` vs `claude-opus-4-7` regression).
    ///
    /// Treat this as the agent-loop equivalent of the previous
    /// `..AgentLoopConfig::default()` pattern: callers fill in only the
    /// fields they care about and inherit the rest from here.
    #[must_use]
    pub fn for_agent(model: impl Into<String>) -> Self {
        Self {
            max_iterations: MAX_ITERATIONS,
            max_tokens: 16_384,
            thinking_budget: None,
            // Matches the default reasoner reqwest timeout (300s /
            // `AURA_MODEL_TIMEOUT_MS`). The previous 60s caused long
            // streams with extended thinking to hit `timeout()` in
            // `call_model` before the provider had a chance to finish
            // — surfacing "Model call timed out after 60s" even though
            // the underlying request was still healthy. Keeping the
            // timeout in one layer (the HTTP client) avoids that
            // split-responsibility race.
            stream_timeout: Duration::from_secs(300),
            billing_reason: "agent_loop".to_string(),
            model_override: None,
            max_context_tokens: Some(200_000),
            credit_budget: None,
            auto_build_cooldown: AUTO_BUILD_COOLDOWN,
            thinking_taper_after: THINKING_TAPER_AFTER,
            thinking_taper_factor: THINKING_TAPER_FACTOR,
            thinking_min_budget: THINKING_MIN_BUDGET,
            extra_tools: Vec::new(),
            system_prompt: String::new(),
            model: model.into(),
            auth_token: None,
            upstream_provider_family: None,
            tool_hints: None,
            aura_project_id: None,
            aura_agent_id: None,
            aura_session_id: None,
            aura_org_id: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            request_kind: ModelRequestKind::Chat,
            observers: Vec::new(),
            intent_classifier: None,
            intent_classifier_manifest: Vec::new(),
            skills_chars: 0,
            subagents_chars: 0,
            phase_reset_signal: None,
            disable_thinking_iteration_0: false,
            max_turns_per_task: aura_core::MAX_TURNS,
            max_iterations_per_task: aura_core::MAX_TURNS,
            stream_event_timeout: Duration::from_secs(90),
            per_tool_timeout: Duration::from_secs(300),
            // E.4 flipped this to `true`. The pump now emits per-
            // `OutputItemDone` block deltas, consults the per-run
            // tool cache, and fires auto-build on writes — closing
            // the parity gaps that kept the pump opt-in in E.3.
            // Operators that need to fall back to the legacy buffered
            // path can flip this back to `false` per call site.
            use_stream_pump: true,
        }
    }
}

/// The main multi-step agent loop orchestrator.
pub struct AgentLoop {
    config: AgentLoopConfig,
}

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
    // make space for the new optional arg. Documented per Rule 1.4.
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
        // [`turn::run_turn_stop_hooks`] to drive the codex-parity
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

    /// Dispatch on the model's stop reason. Returns `true` if the loop should break.
    async fn apply_summary_compaction(
        &self,
        provider: &dyn ModelProvider,
        tools: &[ToolDefinition],
        _event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
        state: &mut LoopState,
        input: aura_compaction::SummaryInput,
    ) {
        if is_cancelled(cancellation_token) {
            return;
        }

        let request = match self.build_summary_request(&input) {
            Ok(request) => request,
            Err(e) => {
                warn!(error = %e, "failed to build compaction summary request");
                return;
            }
        };

        let response = match self
            .call_model(provider, request, None, cancellation_token)
            .await
        {
            Ok(response) => response,
            Err(e) => {
                warn!(error = %summary_error_for_log(&e), "compaction summary generation failed; continuing with local compaction");
                return;
            }
        };

        let summary_text = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if summary_text.trim().is_empty() {
            warn!(
                "compaction summary generation returned no text; continuing with local compaction"
            );
            return;
        }

        context::apply_summary_output(
            &self.config,
            state,
            tools,
            aura_compaction::SummaryOutput::Messages {
                text: summary_text,
                compact_start: input.compact_start,
                compact_end: input.compact_end,
            },
        );
    }

    fn build_summary_request(
        &self,
        input: &aura_compaction::SummaryInput,
    ) -> Result<ModelRequest, crate::AgentError> {
        let prompt =
            crate::prompts::auxiliary::compaction::build_compact_summary_user_prompt(input);
        let max_tokens = (input.max_summary_chars / CHARS_PER_TOKEN)
            .clamp(256, 4_096)
            .try_into()
            .unwrap_or(4_096);

        ModelRequest::builder(
            &self.config.model,
            crate::prompts::auxiliary::compaction::COMPACTION_SUMMARY_SYSTEM_PROMPT,
        )
        .messages(vec![Message::user(prompt)])
        .tools(Vec::new())
        .tool_choice(ToolChoice::None)
        .max_tokens(max_tokens)
        // Force extended thinking *off* on the compaction-summary
        // call. The earlier `Medium` pin (added so the console row
        // didn't render `thinking off` for parity with the dev-loop
        // policy) interacts badly with the tight `max_tokens` clamp
        // above (256..=4_096): on Claude 4.x with `adaptive` thinking,
        // the model consumes the entire budget on a thinking block
        // and returns an empty text body. `apply_summary_compaction`
        // then hits its empty-text early-return, the messages are
        // never reduced, and `compact_if_needed` re-fires `NeedsSummary`
        // on the next iteration — doubling the outbound API call rate
        // for the rest of the task while doing no actual compaction.
        // The summary call is mechanical (rewrite N kB of transcript
        // into ~M chars); thinking-off is the right policy and the
        // console renders it as such.
        .thinking_effort(Some(ThinkingEffort::Off))
        .auth_token(self.config.auth_token.clone())
        .upstream_provider_family(self.config.upstream_provider_family.clone())
        .aura_project_id(self.config.aura_project_id.clone())
        .aura_agent_id(self.config.aura_agent_id.clone())
        .aura_session_id(self.config.aura_session_id.clone())
        .aura_org_id(self.config.aura_org_id.clone())
        .prompt_cache_key(self.config.prompt_cache_key.clone())
        .prompt_cache_retention(parse_cache_retention(
            self.config.prompt_cache_retention.as_deref(),
        ))
        .request_kind(ModelRequestKind::Auxiliary)
        .try_build()
        .map_err(crate::AgentError::from)
    }

    /// Dispatch on the model's stop reason. Returns `true` if the loop should break.
    ///
    /// Codex parity (`codex-rs/core/src/tasks/regular.rs:73-88`):
    /// the model owns the exit signal. `EndTurn` / `StopSequence`
    /// always terminate the loop; `MaxTokens` terminates unless
    /// `handle_max_tokens` synthesised pending tool_use blocks that
    /// the model needs to retry.
    async fn dispatch_stop_reason(
        &self,
        response: &aura_reasoner::ModelResponse,
        executor: &dyn AgentToolExecutor,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        state: &mut LoopState,
    ) -> bool {
        match response.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => true,
            StopReason::MaxTokens => !iteration::handle_max_tokens(&self.config, response, state),
            StopReason::ToolUse => {
                tool_execution::handle_tool_use(self, response, executor, event_tx, state).await
            }
        }
    }

    #[allow(clippy::too_many_arguments)] // TODO(W3): regroup retry inputs behind a `RetryCtx` struct.
    async fn retry_after_context_overflow(
        &self,
        provider: &dyn ModelProvider,
        tools: &[ToolDefinition],
        iteration: usize,
        event_tx: Option<&Sender<AgentLoopEvent>>,
        cancellation_token: Option<&CancellationToken>,
        state: &mut LoopState,
        initial_error: String,
    ) -> Result<aura_reasoner::ModelResponse, iteration::LlmCallError> {
        let recovery_steps = [
            (
                aura_compaction::CompactionConfig::aggressive(),
                "Context limit reached; compacting older context, trimming response budget, and retrying.",
            ),
            (
                aura_compaction::CompactionConfig::micro(),
                "Context is still too large; applying emergency compaction, trimming response budget again, and retrying.",
            ),
        ];
        let mut last_error = initial_error;

        for (tier, warning) in recovery_steps {
            if !context::compact_for_overflow(&self.config, state, tier, tools) {
                debug!("Skipping overflow retry because compaction made no progress");
                continue;
            }

            state.thinking.budget =
                (state.thinking.budget / 2).max(self.config.thinking_min_budget);
            streaming::emit(event_tx, AgentLoopEvent::Warning(warning.to_string()));

            let request = state
                .build_request(&self.config, tools, iteration)
                .map_err(|e| iteration::LlmCallError::Fatal(e.to_string()))?;
            match self
                .call_model(provider, request, event_tx, cancellation_token)
                .await
            {
                Ok(response) => return Ok(response),
                Err(iteration::LlmCallError::PromptTooLong(msg)) => {
                    last_error = msg;
                }
                Err(other) => return Err(other),
            }
        }

        Err(iteration::LlmCallError::PromptTooLong(last_error))
    }
}

/// Tool-result memoization shared by [`super::tool_execution`] and the
/// fuzzy-search lookup in [`super::search_cache`].
///
/// The `exact` / `fuzzy` maps remain the primary read-side hits.
/// Phase 1 of the reread-efficiency plan adds two extra indices:
///
/// * [`Self::read_file_by_path`] is a per-path range index over
///   `read_file` results. On an exact-key miss for `read_file`, the
///   loop consults this vec for a previously-cached window that
///   *contains* the requested `start_line..=end_line`, and slices it
///   in-memory instead of re-running the tool. The slicing layer is
///   intentionally conservative: it does not touch the disk, and it
///   re-uses the superset entry's `content_hash` so downstream
///   compaction dedup can fold the subset response back into the
///   original read.
///
/// Path-scoped invalidation: on a successful write, `update_cache`
/// no longer wipes both maps wholesale. It drops only the cache
/// entries whose path equals, parents, or descends the written path;
/// `search_code` / `find_files` entries still invalidate
/// workspace-wide because their results are not path-scoped.
#[derive(Default)]
pub(crate) struct ToolResultCache {
    /// Exact-key cache: `tool_name + canonical_input_json`.
    pub(crate) exact: HashMap<String, String>,
    /// Secondary, normalized index for `search_code` / `find_files`
    /// that collapses alternation-order and trivial whitespace
    /// variants. Populated alongside `exact` in `update_cache`;
    /// consulted only on a miss of the exact key. Cleared together
    /// with the workspace-global slice of `exact` on any successful
    /// write so the "write invalidates search" invariant is preserved.
    pub(crate) fuzzy: HashMap<String, String>,
    /// Per-path range index over `read_file` results. Keyed by the
    /// canonical (forward-slash, no trailing slash, `./` stripped)
    /// path string. Each entry records the window the call returned
    /// plus the rendered tool output so a later subset request can be
    /// served without disk I/O.
    pub(crate) read_file_by_path: HashMap<String, Vec<ReadRangeEntry>>,
}

/// One cached `read_file` result, indexed by path in
/// [`ToolResultCache::read_file_by_path`].
///
/// We store the rendered tool output (the exact bytes the model saw).
/// Slicing for a subset request lifts lines out of `rendered` by
/// their leading `{:>6}|` line-number prefix — no `fs::read` call, no
/// second pass through the tool. Whole-file entries (`start_line` and
/// `end_line` both `None`) carry the raw bytes in `rendered` and are
/// re-rendered in memory on demand.
#[derive(Debug, Clone)]
pub(crate) struct ReadRangeEntry {
    pub(crate) start_line: Option<usize>,
    pub(crate) end_line: Option<usize>,
    pub(crate) rendered: String,
}

/// Per-iteration response-token budget and the one-shot "skip the
/// taper next iteration" override.
///
/// Held as its own struct so [`super::iteration::handle_max_tokens`]
/// only has to mutate `state.thinking.restore_next_iteration` (a
/// single boolean) without taking a `&mut LoopState` that grants
/// access to message lists, caches, etc.
pub(crate) struct ThinkingBudget {
    /// Tokens the loop allows for the next streaming response. Taper
    /// applies in [`LoopState::begin_iteration`] once the iteration
    /// counter passes [`AgentLoopConfig::thinking_taper_after`].
    pub(crate) budget: u32,
    /// Set by [`super::iteration::handle_max_tokens`] when the previous
    /// turn ended with pending tool_use blocks truncated by
    /// `max_tokens`. The next [`LoopState::begin_iteration`] observes
    /// this flag and restores `budget` to `config.max_tokens`
    /// (skipping the taper for that one iteration) so the retry has
    /// the full budget it needs to re-emit the dropped tool call.
    /// Cleared immediately after the restore so subsequent iterations
    /// resume normal tapering.
    pub(crate) restore_next_iteration: bool,
    /// One-shot flag: when `true`, [`LoopState::build_request`] caps
    /// `max_tokens` at the auto-thinking threshold so the underlying
    /// reasoner does NOT auto-enable extended thinking for that one
    /// turn, then resets the flag.
    ///
    /// Set by [`LoopState::begin_iteration`] for `iteration == 0`
    /// (the explore turn should be fast tool calls, not multi-minute
    /// deliberation) and by the read-only force-tool path (Anthropic
    /// blocks forced tool use while extended thinking is enabled, so
    /// the two flips ride together).
    pub(crate) disable_thinking_this_iteration: bool,
    /// Latch armed by the dispatch path when the dev-loop intercept
    /// fires on a `MaxTokens` stop reason with no pending tool calls
    /// (i.e. extended thinking consumed the entire response budget
    /// without producing a tool_use block). The next
    /// [`LoopState::begin_iteration`] consumes-and-clears this latch
    /// into [`Self::disable_thinking_this_iteration`] so the recovery
    /// turn opens with thinking disabled — the model emits a tool
    /// call instead of more deliberation.
    ///
    /// We need a latch (not a same-iteration flip) because
    /// [`LoopState::begin_iteration`] unconditionally clears
    /// `disable_thinking_this_iteration` at the top of every turn:
    /// a flag armed at the END of iteration N is wiped at the TOP of
    /// iteration N+1 before `build_request` ever sees it.
    pub(crate) pending_disable_thinking_next_iteration: bool,
}

/// Mutable state carried across iterations of the agent loop.
pub(crate) struct LoopState {
    pub(crate) result: AgentLoopResult,
    pub(crate) tool_cache: ToolResultCache,
    pub(crate) exploration_state: ExplorationState,
    pub(crate) budget_state: BudgetState,
    pub(crate) had_any_write: bool,
    /// Set true the first iteration whose tool results contain any
    /// `FileOp` (any successful `write_file` / `edit_file` /
    /// `delete_file`). Cumulative across the run — never reset.
    /// Consumed by the reasoning-effort policy to drop to `Low`
    /// effort once forward motion has happened.
    pub(crate) had_any_file_write: bool,
    /// Set true when `handle_task_done` successfully returns
    /// `stop_loop = true` (i.e. all DoD gates passed). Cumulative
    /// across the run — never reset.
    ///
    /// Wired in `tool_execution::check_termination_conditions` by
    /// observing a non-error tool result whose source tool is
    /// `task_done` and whose `stop_loop` flag is set.
    pub(crate) task_done_completed: bool,
    /// Phase 2: set to `true` the iteration after a successful
    /// `submit_plan` accept has been observed via
    /// [`AgentLoopConfig::phase_reset_signal`]. Cumulative across the
    /// run — never reset. Drives [`Self::compute_thinking_effort`] to
    /// drop to `Low` once a plan exists, mirroring codex's
    /// post-plan `reasoning.effort=low` behaviour.
    ///
    /// We deliberately reuse the existing `phase_reset_signal`
    /// handshake (set by `handle_submit_plan` in the task executor)
    /// instead of inventing a parallel signal path. The first
    /// iteration's flip is the task-start pre-seed (see
    /// `agent_runner::execute_task_tracked`), so we only treat
    /// observations on `iteration > 0` as real submit_plan acceptances.
    pub(crate) submit_plan_called: bool,
    pub(crate) checkpoint_emitted: bool,
    pub(crate) exploration_compaction_done: bool,
    pub(crate) build_cooldown: usize,
    pub(crate) thinking: ThinkingBudget,
    pub(crate) last_context_tokens_estimate: Option<u64>,
    pub(crate) messages: Vec<Message>,
    pub(crate) build_baseline: Option<BuildBaseline>,
    /// Per-iteration net file-op accumulator. Reset at the top of
    /// every iteration. Tracks writes so the
    /// `had_any_file_write` latch lights up via
    /// `tool_pipeline::track_tool_effects` and tool-result caching
    /// invariants stay path-aware.
    pub(crate) turn_diff: turn_diff::TurnDiff,
    /// Per-turn tracker for identical-byte re-reads (Phase 3b).
    pub(crate) repeated_read_tracker: crate::prompts::steering::RepeatedReadTracker,
    /// Paths successfully read this session; used by the duplicate-read gate.
    pub(crate) session_read_paths: std::collections::HashSet<PathBuf>,
    /// Per-path read budget granted after a successful write to that path.
    /// Lets the agent inspect changed regions while repairing malformed edits.
    pub(crate) read_after_write_allowances: std::collections::HashMap<PathBuf, u8>,
    /// One-shot latch: [`crate::prompts::steering::evaluate_implement_now`] fired
    /// for this run.
    pub(crate) implement_now_injected: bool,
}

impl LoopState {
    fn new(config: &AgentLoopConfig, messages: Vec<Message>) -> Self {
        Self {
            result: AgentLoopResult::default(),
            tool_cache: ToolResultCache::default(),
            exploration_state: ExplorationState::default(),
            budget_state: BudgetState::default(),
            had_any_write: false,
            had_any_file_write: false,
            task_done_completed: false,
            submit_plan_called: false,
            checkpoint_emitted: false,
            exploration_compaction_done: false,
            build_cooldown: 0,
            thinking: ThinkingBudget {
                // Seed from `thinking_budget` when present so the runner
                // can request a smaller starting budget than the
                // per-request `max_tokens` ceiling. Truncation recovery
                // in `begin_iteration` still restores to `max_tokens`.
                budget: config.thinking_budget.unwrap_or(config.max_tokens),
                restore_next_iteration: false,
                disable_thinking_this_iteration: false,
                pending_disable_thinking_next_iteration: false,
            },
            last_context_tokens_estimate: None,
            messages,
            build_baseline: None,
            turn_diff: turn_diff::TurnDiff::default(),
            repeated_read_tracker: crate::prompts::steering::RepeatedReadTracker::new(),
            session_read_paths: std::collections::HashSet::new(),
            read_after_write_allowances: std::collections::HashMap::new(),
            implement_now_injected: false,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_for_tests(config: &AgentLoopConfig, messages: Vec<Message>) -> Self {
        Self::new(config, messages)
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn begin_iteration(&mut self, config: &AgentLoopConfig, iteration: usize) {
        self.build_cooldown = self.build_cooldown.saturating_sub(1);

        // Phase 1.A: scope the turn-diff to the iteration we are about
        // to execute. The previous iteration's net file ops are no
        // longer relevant — `had_any_file_write` (cumulative latch)
        // and the per-iteration `turn_diff` answer different questions.
        self.turn_diff.reset();

        for kind in self.repeated_read_tracker.begin_turn() {
            crate::prompts::steering::SteeringInjector::inject(&mut self.messages, kind);
        }

        if let Some(kind) = crate::prompts::steering::evaluate_implement_now(config, self) {
            crate::prompts::steering::SteeringInjector::inject(&mut self.messages, kind);
            self.implement_now_injected = true;
        }

        // One-shot extended-thinking disable flag is re-evaluated each
        // iteration: seeded from the cross-iteration latch (armed by
        // the dispatch path's MaxTokens-empty intercept), then
        // re-set below for the iteration-0 explore case. `build_request`
        // reads the flag to decide whether to clamp `max_tokens` below
        // the auto-thinking threshold. The latch is consume-and-clear
        // so it fires at most once per arm.
        self.thinking.disable_thinking_this_iteration =
            self.thinking.pending_disable_thinking_next_iteration;
        self.thinking.pending_disable_thinking_next_iteration = false;

        // Observe-and-clear the optional handshake from a wrapping
        // `TaskToolExecutor`: when `submit_plan` is accepted the
        // executor flips this shared `Arc<AtomicBool>` to `true`, and
        // the loop must zero out the exploration counter so the
        // implement phase has a fresh budget instead of inheriting the
        // exploration phase's exhausted one.
        // `exploration_compaction_done` is cleared so proactive
        // compaction can fire once more during the implement phase.
        if let Some(ref signal) = config.phase_reset_signal {
            if signal.swap(false, Ordering::AcqRel) {
                tracing::info!(
                    old_exploration_count = self.exploration_state.count,
                    "submit_plan accepted: resetting exploration counter"
                );
                self.exploration_state.count = 0;
                self.exploration_compaction_done = false;
                // Phase 2: latch the "submit_plan was accepted" signal
                // for the effort policy. The reset signal is also flipped
                // at task start (pre-seeded `true` in
                // `agent_runner::execute_task_tracked` so the first
                // iteration's reset path fires), so we only treat
                // observations on `iteration > 0` as real submit_plan
                // acceptances. Iteration 0's flip is the task-start
                // pre-seed and must not toggle the effort policy.
                if iteration > 0 {
                    self.submit_plan_called = true;
                }
            }
        }

        // Temporary (2026-05): the dev-loop policy now pins
        // reasoning effort to `Medium` across every iteration (see
        // `compute_thinking_effort`). The previous iteration-0
        // `max_tokens` clamp — armed here when
        // `disable_thinking_iteration_0` was set — has been removed
        // because it contradicted that pin: a 2048-token cap on the
        // explore turn either rejects the Anthropic request outright
        // (Claude 3.7 `enabled` mode wants `budget_tokens=4096` for
        // Medium) or leaves Adaptive thinking with no real budget to
        // deliberate inside. The cross-iteration recovery latch
        // [`ThinkingBudget::pending_disable_thinking_next_iteration`]
        // is currently never armed; keeping the consume-and-clear
        // wiring above costs nothing and preserves an obvious revert
        // path if we decide to bring the clamp back later.

        // If the previous iteration ended with a `MaxTokens` truncation
        // mid-`tool_use`, restore the budget to the configured maximum
        // and skip the taper this turn. The model is about to retry
        // the dropped tool call and needs the full budget to fit the
        // JSON that previously got cut off. Tapering resumes on the
        // iteration after (the flag is cleared here so it fires at
        // most once per truncation).
        if self.thinking.restore_next_iteration {
            self.thinking.budget = config.max_tokens;
            self.thinking.restore_next_iteration = false;
            return;
        }

        if iteration >= config.thinking_taper_after {
            self.thinking.budget =
                (f64::from(self.thinking.budget) * config.thinking_taper_factor) as u32;
            self.thinking.budget = self.thinking.budget.max(config.thinking_min_budget);
        }
    }

    /// Reasoning-effort policy applied per iteration.
    /// Codex sets `reasoning.effort` explicitly per Responses API call
    /// (codex-rs/core/src/client.rs:698-714); the rules below are the
    /// aura analog tailored to aura's `write_file`/`edit_file`/
    /// `delete_file` surface.
    ///
    /// **Temporary (2026-05): dev-loop turns are pinned to
    /// [`ThinkingEffort::Medium`] regardless of iteration / write /
    /// plan state.** We're evaluating whether holding a single effort
    /// level across the run converges faster than the codex-style
    /// `Off → Medium → Low` taper. `disable_thinking_iteration_0` is
    /// only set by `configure_loop_config` for dev-loop tasks, so chat
    /// and other callers retain the original tiered policy below.
    ///
    /// Resolution order for non-dev-loop callers (first match wins):
    ///
    /// 1. Iteration 0 → `Medium` (analysis turn).
    /// 2. `had_any_file_write` → `Low` (forward motion has happened,
    ///    cap the deliberation budget).
    /// 3. `submit_plan_called` → `Low` (the plan exists; codex drops
    ///    to low effort once the agent is committed to an
    ///    implementation phase).
    /// 4. Otherwise → `Medium`.
    fn compute_thinking_effort(
        &self,
        config: &AgentLoopConfig,
        iteration: usize,
    ) -> ThinkingEffort {
        if config.disable_thinking_iteration_0 {
            return ThinkingEffort::Medium;
        }
        if iteration == 0 {
            return ThinkingEffort::Medium;
        }
        if self.had_any_file_write || self.submit_plan_called {
            return ThinkingEffort::Low;
        }
        ThinkingEffort::Medium
    }

    fn build_request(
        &self,
        config: &AgentLoopConfig,
        tools: &[ToolDefinition],
        iteration: usize,
    ) -> Result<ModelRequest, crate::AgentError> {
        // Phase 3: narrow `tools` down to domain-relevant entries before the
        // tool-hints logic runs. The classifier is keyed on the most recent
        // pure-text user message, so scratchpad tool-result turns reuse the
        // previous filter rather than widening the surface back to every tool.
        let classifier_filtered: Vec<ToolDefinition> = match (
            config.intent_classifier.as_deref(),
            latest_user_text(&self.messages),
        ) {
            (Some(classifier), Some(text)) if !config.intent_classifier_manifest.is_empty() => {
                classifier.filter_tools(text, &config.intent_classifier_manifest, tools)
            }
            _ => tools.to_vec(),
        };

        let effective_tools = match &config.tool_hints {
            Some(hints) if !hints.is_empty() => {
                let filtered: Vec<_> = classifier_filtered
                    .iter()
                    .filter(|t| hints.iter().any(|h| h == &t.name))
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    classifier_filtered
                } else {
                    filtered
                }
            }
            _ => classifier_filtered,
        };
        // The cook-loop-fix strip (2026-05) removed the read-only
        // streak counter and the force-tool-choice path that rode on
        // top of it. `tool_choice` is always `Auto`; the model picks
        // its own next move.
        let tool_choice = aura_reasoner::ToolChoice::Auto;

        let has_task_tools = effective_tools.iter().any(|tool| {
            matches!(
                tool.name.as_str(),
                "create_task" | "update_task" | "list_tasks" | "get_task" | "delete_task"
            )
        });
        let has_spec_tools = effective_tools.iter().any(|tool| {
            matches!(
                tool.name.as_str(),
                "create_spec" | "update_spec" | "list_specs" | "get_spec" | "delete_spec"
            )
        });
        // Narrow the project-tool override to dev-loop turns only.
        //
        // The `ProjectToolTaskExtract` / `ProjectToolSpecGen` request kinds
        // carry a `PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES = 48 KiB` cap in
        // `aura-reasoner::content_profile`. The cap exists so the
        // task-extraction phase of the dev loop can't blow up the model
        // request with arbitrary chat history. The previous wildcard
        // arm — `(true, _, _, _) => ProjectToolTaskExtract` — clobbered
        // any explicit `config.request_kind` (including `Chat`) whenever
        // the task tools happened to be visible. That makes every chat
        // turn for an agent with `create_task`/etc. in scope hard-fail
        // with `EmergencyCapRequired` once history accumulates past
        // ~48 KiB, even though normal chat conversations should be
        // governed by the much-larger chat budget instead.
        //
        // Restrict the override to `DevLoopBootstrap`/`Continuation`
        // request kinds, where the task-extraction context invariant
        // actually applies. Plain `Chat` / `Auxiliary` requests now keep
        // their declared `config.request_kind` even when they happen to
        // have task / spec tools available.
        let request_kind = match (
            has_task_tools,
            has_spec_tools,
            config.request_kind,
            iteration,
        ) {
            (
                true,
                _,
                ModelRequestKind::DevLoopBootstrap | ModelRequestKind::DevLoopContinuation,
                _,
            ) => ModelRequestKind::ProjectToolTaskExtract,
            (
                _,
                true,
                ModelRequestKind::DevLoopBootstrap | ModelRequestKind::DevLoopContinuation,
                _,
            ) => ModelRequestKind::ProjectToolSpecGen,
            (_, _, ModelRequestKind::DevLoopBootstrap, 0) => ModelRequestKind::DevLoopBootstrap,
            (_, _, ModelRequestKind::DevLoopBootstrap, _) => ModelRequestKind::DevLoopContinuation,
            (_, _, kind, _) => kind,
        };

        // Disable extended thinking for this one iteration by clamping
        // `max_tokens` below the reasoner's auto-thinking threshold
        // (`> 2048`, see
        // `aura_reasoner::anthropic::convert::resolve_thinking`).
        // The reasoner does not currently expose a per-request
        // "extended thinking off" toggle for Claude 4.x — it
        // auto-enables thinking whenever `max_tokens > 2048` — so the
        // only correctness path is to keep `max_tokens` at or below
        // that threshold.
        //
        // The flag persists for the whole iteration: it is set in
        // [`Self::begin_iteration`] and cleared at the top of the
        // NEXT [`Self::begin_iteration`] call. That keeps the
        // disable in force across an overflow-retry within the same
        // iteration (`retry_after_context_overflow` calls
        // `build_request` again without re-entering
        // `begin_iteration`).
        //
        // TODO(harness-v2): once `aura-reasoner` exposes an explicit
        // "thinking: off" knob, replace this clamp with a direct call
        // to disable extended thinking and remove the implicit
        // coupling between `max_tokens` and the thinking switch.
        let effective_max_tokens = if self.thinking.disable_thinking_this_iteration {
            self.thinking.budget.min(THINKING_AUTO_ENABLE_THRESHOLD)
        } else {
            self.thinking.budget
        };

        // Codex parity: emit an explicit `reasoning.effort` on every
        // request. The reasoner's `max_tokens > 2048` auto-enable
        // path stays as a fallback for providers that ignore the
        // explicit field.
        let thinking_effort = Some(self.compute_thinking_effort(config, iteration));

        ModelRequest::builder(&config.model, &config.system_prompt)
            .messages(self.messages.clone())
            .tools(effective_tools)
            .tool_choice(tool_choice)
            .max_tokens(effective_max_tokens)
            .thinking_effort(thinking_effort)
            .auth_token(config.auth_token.clone())
            .upstream_provider_family(config.upstream_provider_family.clone())
            .aura_project_id(config.aura_project_id.clone())
            .aura_agent_id(config.aura_agent_id.clone())
            .aura_session_id(config.aura_session_id.clone())
            .aura_org_id(config.aura_org_id.clone())
            .prompt_cache_key(config.prompt_cache_key.clone())
            .prompt_cache_retention(parse_cache_retention(
                config.prompt_cache_retention.as_deref(),
            ))
            .request_kind(request_kind)
            .try_build()
            .map_err(crate::AgentError::from)
    }
}

/// Layer E.1 helper retained as a free function (rather than a
/// `Option::is_some_and` call at each site) so that
/// [`sampling::run_sampling_request`], [`turn::run_turn`], and the
/// pre-E.1 entry points all share one branch-free probe.
pub(crate) fn is_cancelled(token: Option<&CancellationToken>) -> bool {
    token.is_some_and(CancellationToken::is_cancelled)
}

fn summary_error_for_log(error: &iteration::LlmCallError) -> &'static str {
    match error {
        iteration::LlmCallError::InsufficientCredits(_) => "insufficient_credits",
        iteration::LlmCallError::PromptTooLong(_) => "prompt_too_long",
        iteration::LlmCallError::RateLimited(_) => "rate_limited",
        iteration::LlmCallError::Fatal(_) => "fatal",
    }
}

/// Return the text of the most recent user-role message whose content is
/// plain text (skipping tool-result turns, which carry tool output rather
/// than a natural-language intent).
///
/// Used by [`LoopState::build_request`] to feed the intent classifier on
/// every iteration — including scratchpad iterations that follow a tool
/// call — so the tool filter stays keyed on the original user intent
/// until the user speaks again.
fn latest_user_text(messages: &[Message]) -> Option<&str> {
    for msg in messages.iter().rev() {
        if matches!(msg.role, Role::User)
            && msg
                .content
                .iter()
                .any(|b| matches!(b, aura_reasoner::ContentBlock::Text { .. }))
        {
            return msg.content.iter().find_map(|b| match b {
                aura_reasoner::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            });
        }
    }
    None
}

/// Parse the wire-level `prompt_cache_retention` string forwarded by
/// `aura-os` into a typed [`aura_reasoner::PromptCacheRetention`]. Unknown
/// or blank values fall through as `None` so the reasoner falls back to
/// the upstream provider default.
fn parse_cache_retention(value: Option<&str>) -> Option<aura_reasoner::PromptCacheRetention> {
    let v = value?.trim();
    match v {
        "24h" | "h24" | "hours_24" | "Hours24" => {
            Some(aura_reasoner::PromptCacheRetention::Hours24)
        }
        "in_memory" | "InMemory" | "memory" => Some(aura_reasoner::PromptCacheRetention::InMemory),
        _ => None,
    }
}

#[cfg(test)]
mod intent_classifier_tests {
    use super::*;
    use aura_reasoner::ToolDefinition;
    use serde_json::json;
    use std::sync::Arc;

    fn mk_tool(name: &str) -> ToolDefinition {
        ToolDefinition::new(name, name, json!({}))
    }

    fn mk_config_with_classifier() -> AgentLoopConfig {
        let classifier = IntentClassifier::from_rules(
            vec!["project".to_string()],
            vec![("billing".to_string(), vec!["credit".to_string()])],
        );
        AgentLoopConfig {
            intent_classifier: Some(Arc::new(classifier)),
            intent_classifier_manifest: vec![
                ("create_project".to_string(), "project".to_string()),
                ("list_credits".to_string(), "billing".to_string()),
            ],
            ..AgentLoopConfig::for_agent("claude-test-model")
        }
    }

    #[test]
    fn build_request_filters_tier2_tools_when_not_triggered() {
        let config = mk_config_with_classifier();
        let state = LoopState::new(&config, vec![Message::user("hello there")]);
        let tools = vec![
            mk_tool("create_project"),
            mk_tool("list_credits"),
            mk_tool("read_file"),
        ];

        let req = state.build_request(&config, &tools, 1).unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();

        assert!(names.contains(&"create_project"), "tier-1 tool kept");
        assert!(names.contains(&"read_file"), "unmapped tool passes through");
        assert!(
            !names.contains(&"list_credits"),
            "tier-2 billing tool hidden"
        );
    }

    #[test]
    fn build_request_admits_tier2_when_keyword_matches() {
        let config = mk_config_with_classifier();
        let state = LoopState::new(
            &config,
            vec![Message::user("check my credit balance please")],
        );
        let tools = vec![mk_tool("create_project"), mk_tool("list_credits")];

        let req = state.build_request(&config, &tools, 1).unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"list_credits"));
        assert!(names.contains(&"create_project"));
    }

    #[test]
    fn build_request_skips_tool_result_messages_when_picking_intent() {
        let config = mk_config_with_classifier();
        let msgs = vec![
            Message::user("check my credit balance"),
            Message::assistant("calling tool"),
            Message::tool_results(vec![(
                "tu_1".into(),
                aura_reasoner::ToolResultContent::Text("100".into()),
                false,
            )]),
        ];
        let state = LoopState::new(&config, msgs);
        let tools = vec![mk_tool("list_credits"), mk_tool("create_project")];

        let req = state.build_request(&config, &tools, 2).unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"list_credits"),
            "classifier should still see original user message after a tool-result turn"
        );
    }

    #[test]
    fn build_request_passthrough_when_classifier_absent() {
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let state = LoopState::new(&config, vec![Message::user("anything")]);
        let tools = vec![mk_tool("anything_tool")];
        let req = state.build_request(&config, &tools, 1).unwrap();
        assert_eq!(req.tools.len(), 1);
    }

    #[test]
    fn build_request_keeps_tool_hints_scoped_after_first_iteration() {
        let config = AgentLoopConfig {
            tool_hints: Some(vec!["read_file".to_string(), "create_task".to_string()]),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let msgs = vec![
            Message::user("extract tasks"),
            Message::assistant("calling tool"),
            Message::tool_results(vec![(
                "tu_1".into(),
                aura_reasoner::ToolResultContent::Text("large requirements".into()),
                false,
            )]),
        ];
        let state = LoopState::new(&config, msgs);
        let tools = vec![
            mk_tool("read_file"),
            mk_tool("create_task"),
            mk_tool("run_command"),
            mk_tool("generate_image"),
        ];

        let req = state.build_request(&config, &tools, 2).unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();

        assert_eq!(names, vec!["read_file", "create_task"]);
        assert!(matches!(req.tool_choice, aura_reasoner::ToolChoice::Auto));
    }

    #[test]
    fn build_request_keeps_tool_hints_auto_on_first_iteration() {
        let config = AgentLoopConfig {
            tool_hints: Some(vec!["read_file".to_string(), "create_task".to_string()]),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let state = LoopState::new(&config, vec![Message::user("extract tasks")]);
        let tools = vec![
            mk_tool("read_file"),
            mk_tool("create_task"),
            mk_tool("run_command"),
        ];

        let req = state.build_request(&config, &tools, 0).unwrap();
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();

        assert_eq!(names, vec!["read_file", "create_task"]);
        assert!(matches!(req.tool_choice, aura_reasoner::ToolChoice::Auto));
    }

    /// Regression: a `Chat` request with `create_task` in scope must
    /// keep `request_kind = Chat`, NOT silently get re-classified as
    /// `ProjectToolTaskExtract`. The latter carries a 48 KiB total-text
    /// budget in `aura-reasoner::content_profile`, so the old
    /// reclassification turned every chat for an agent-with-task-tools
    /// into a hard `EmergencyCapRequired` failure once history grew
    /// past ~48 KiB. The fix narrows the override to dev-loop turns.
    #[test]
    fn build_request_keeps_chat_kind_when_task_tools_visible() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::Chat,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let state = LoopState::new(&config, vec![Message::user("hi there")]);
        let tools = vec![mk_tool("create_task"), mk_tool("read_file")];

        let req = state.build_request(&config, &tools, 0).unwrap();
        assert_eq!(
            req.metadata.kind,
            Some(ModelRequestKind::Chat),
            "Chat must stay Chat even when task tools are visible (otherwise EmergencyCapRequired blocks chat at 48 KiB)"
        );
    }

    /// Companion: same invariant for spec tools — `create_spec` etc.
    /// in scope must not flip a `Chat` turn into `ProjectToolSpecGen`.
    #[test]
    fn build_request_keeps_chat_kind_when_spec_tools_visible() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::Chat,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let state = LoopState::new(&config, vec![Message::user("hi")]);
        let tools = vec![mk_tool("create_spec"), mk_tool("read_file")];

        let req = state.build_request(&config, &tools, 0).unwrap();
        assert_eq!(req.metadata.kind, Some(ModelRequestKind::Chat));
    }

    /// The dev-loop flow IS still subject to the project-tool override:
    /// when the caller declares `DevLoopBootstrap` AND task tools are
    /// available, the iteration after iteration `0` must report
    /// `ProjectToolTaskExtract` (the existing extraction-phase guard).
    /// Pins the narrowing didn't accidentally break the dev loop.
    #[test]
    fn build_request_promotes_devloop_to_project_tool_task_extract_when_task_tools_visible() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::DevLoopBootstrap,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let state = LoopState::new(&config, vec![Message::user("extract tasks")]);
        let tools = vec![mk_tool("create_task")];

        let req = state.build_request(&config, &tools, 1).unwrap();
        assert_eq!(
            req.metadata.kind,
            Some(ModelRequestKind::ProjectToolTaskExtract)
        );
    }

    /// Mirror for the spec branch.
    #[test]
    fn build_request_promotes_devloop_to_project_tool_spec_gen_when_spec_tools_visible() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::DevLoopBootstrap,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let state = LoopState::new(&config, vec![Message::user("extract specs")]);
        let tools = vec![mk_tool("create_spec")];

        let req = state.build_request(&config, &tools, 1).unwrap();
        assert_eq!(
            req.metadata.kind,
            Some(ModelRequestKind::ProjectToolSpecGen)
        );
    }
}

#[cfg(test)]
mod summary_request_tests {
    use super::*;
    use aura_compaction::SummaryInput;

    fn sample_summary_input() -> SummaryInput {
        SummaryInput {
            compact_start: 0,
            compact_end: 1,
            compactable_messages: vec![Message::user("first")],
            recent_tail: vec![Message::assistant("latest")],
            original_chars: 5_000,
            local_chars: 4_000,
            target_total_chars: 2_000,
            max_summary_chars: 1_000,
        }
    }

    /// Regression: the auxiliary compaction-summary call must ship
    /// `thinking_effort = Off`. Setting it to `Medium` (which a prior
    /// WIP change tried, for parity with the dev-loop thinking pin)
    /// interacts badly with the tight `max_tokens` clamp (256..=4_096):
    /// Claude 4.x with adaptive thinking burns the entire budget on a
    /// thinking block and returns an empty text body, which makes
    /// `apply_summary_compaction` early-return without ever shrinking
    /// the transcript. `compact_if_needed` then re-fires `NeedsSummary`
    /// on every subsequent iteration, doubling the outbound API call
    /// rate for the rest of the task while doing no actual compaction.
    /// The companion fix in `effective_compaction_request_kind`
    /// addresses *why* `NeedsSummary` was firing every turn, but the
    /// summary call itself must also be able to produce real output.
    #[test]
    fn build_summary_request_disables_thinking() {
        let config = AgentLoopConfig::for_agent("aura-claude-opus-4-7");
        let agent = AgentLoop::new(config);
        let input = sample_summary_input();

        let request = agent
            .build_summary_request(&input)
            .expect("summary request builder must succeed for valid inputs");

        assert_eq!(
            request.thinking_effort,
            Some(ThinkingEffort::Off),
            "compaction-summary call must NOT enable extended thinking; the \
             tight max_tokens clamp would otherwise starve the actual summary \
             output (see comment on the .thinking_effort(..) line)"
        );
    }

    /// The summary call is single-shot: one user message, zero tools.
    /// Pins the request shape so an accidental tool-list bleed-through
    /// or extra messages from the live transcript would fail loudly.
    #[test]
    fn build_summary_request_ships_clean_single_shot_payload() {
        let config = AgentLoopConfig::for_agent("aura-claude-opus-4-7");
        let agent = AgentLoop::new(config);
        let input = sample_summary_input();

        let request = agent.build_summary_request(&input).unwrap();

        assert_eq!(request.messages.len(), 1, "exactly one user message");
        assert_eq!(request.tools.len(), 0, "no tools attached");
        assert!(matches!(request.tool_choice, ToolChoice::None));
        assert_eq!(request.metadata.kind, Some(ModelRequestKind::Auxiliary));
    }
}
