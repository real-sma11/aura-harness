//! Main agent loop orchestrator.
//!
//! `AgentLoop` drives the multi-step agentic conversation by calling
//! the model provider in a loop with intelligence: blocking detection,
//! compaction, sanitization, budget management, etc.

mod context;
mod continuation;
mod iteration;
mod sampling;
mod search_cache;
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
mod streaming_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_advanced;

use std::collections::HashMap;
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
    /// Defaults to [`crate::constants::MAX_ITERATIONS`] (`usize::MAX`,
    /// effectively unlimited). The agent loop short-circuits the
    /// iteration check and the utilization-based budget warnings when
    /// this is `usize::MAX`, so the turn ends only on `EndTurn` from
    /// the model, exhaustion of [`Self::credit_budget`], stall
    /// detection, or cooperative cancellation. Callers (e.g.
    /// `aura_runtime::session::state::Session::agent_loop_config`)
    /// that bridge a wire-protocol `u32` should map `u32::MAX` →
    /// `usize::MAX` to engage the unlimited-mode short-circuits.
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
    /// Disable Anthropic extended thinking on iteration 0.
    ///
    /// When `true` (the policy used by [`crate::agent_runner`] for
    /// dev-loop tasks), [`LoopState::begin_iteration`] arms the
    /// [`ThinkingBudget::disable_thinking_this_iteration`] flag for
    /// the very first turn so the explore phase emits fast tool
    /// calls instead of "Thought for 2m"-bursting before the first
    /// `read_file`. Default `false` for chat / generic callers
    /// where deliberation on the first turn is desirable.
    pub disable_thinking_iteration_0: bool,
    /// Marker for the dev-loop task profile.
    ///
    /// Historically gated the `EndTurn` intercept escalation and
    /// force-tool-choice path; the cook-loop-fix strip (2026-05)
    /// removed both. The flag is retained as a profile marker so
    /// downstream consumers (system-prompt builder, telemetry,
    /// log tagging) can still distinguish dev-loop runs from chat /
    /// generic runs, but it no longer influences the agent loop's
    /// termination behavior — `EndTurn` always terminates the loop.
    pub dev_loop_completion_required: bool,
    /// Phase 1.B: hard cap on the number of consecutive-no-write
    /// continuation prompts the loop will inject before failing the
    /// task with `task_blocked`. Default `6` (matches codex's
    /// `continuation.md` ergonomics: 2 soft nudges + up to 4 blocked
    /// audits before giving up). Only consulted when
    /// [`Self::dev_loop_completion_required`] is true.
    pub max_continuation_turns: u32,
    /// Layer E.1: hard cap on the number of *turns* one task may run.
    /// A turn is the unit of work between "model starts talking" and
    /// "model goes quiet without follow-up signal"; codex's
    /// `regular.rs` task shell loops on turns until `input_queue` is
    /// empty. E.1 has no `input_queue` yet (lands in E.2), so the
    /// task shell runs exactly one turn per task and this cap is
    /// effectively dormant. The default `50` matches codex's
    /// recommended ergonomics: most user-driven sessions converge in
    /// <10 turns; the cap exists to surface a typed
    /// [`AgentError::TurnBudgetExceeded`](crate::AgentError::TurnBudgetExceeded)
    /// instead of letting a runaway `input_queue` push the agent
    /// indefinitely.
    pub max_turns_per_task: u32,
    /// Layer E.1: hard cap on the *total* number of sampling
    /// requests (model round-trips) across every turn of one task.
    /// Independent of [`Self::max_iterations`] — that knob remains the
    /// pre-E.1 global ceiling (default `usize::MAX` to avoid the
    /// silent-cancel regression that the historic 25-cap caused for
    /// long-running batch workflows). This cap defaults to `500` so
    /// it stays well above the steady-state working size of every
    /// existing dev-loop / chat scenario but trips on a genuine
    /// runaway. Trips surface as
    /// [`AgentError::TurnBudgetExceeded`](crate::AgentError::TurnBudgetExceeded).
    pub max_iterations_per_task: u32,
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

impl Default for AgentLoopConfig {
    fn default() -> Self {
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
            model: crate::constants::DEFAULT_MODEL.to_string(),
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
            dev_loop_completion_required: false,
            max_continuation_turns: 6,
            max_turns_per_task: 50,
            max_iterations_per_task: 500,
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
        let input_queue = handle.map(crate::AgentRunnerHandle::queue);
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
            input_queue,
        );
        match observer {
            Some(obs) => aura_reasoner::DEBUG_RETRY_OBSERVER.scope(obs, fut).await,
            None => fut.await,
        }
    }

    // E.2: mirrors `run_with_session`'s arg list (one over the
    // clippy default ceiling). Same justification: the `input_queue`
    // is the only new arg, and packing it into a struct would
    // require every helper inside this module to learn a new wrapper
    // type. Documented per Rule 1.4.
    #[allow(clippy::too_many_arguments)]
    async fn run_inner(
        &self,
        provider: &dyn ModelProvider,
        executor: &dyn AgentToolExecutor,
        messages: Vec<Message>,
        tools: Vec<ToolDefinition>,
        event_tx: Option<Sender<AgentLoopEvent>>,
        cancellation_token: Option<CancellationToken>,
        input_queue: Option<Arc<crate::session::input_queue::InputQueue>>,
    ) -> Result<AgentLoopResult, crate::AgentError> {
        // Layer E.1 + E.2: delegate to the nested task → turn → sampling
        // topology. The old per-iteration `for` loop body now lives
        // in [`sampling::run_sampling_request`]; the turn-level
        // `needs_follow_up` predicate and Phase 1.B stop hooks live
        // in [`turn::run_turn`] / [`turn::run_turn_stop_hooks`]; the
        // outer task shell with `max_turns_per_task` /
        // `max_iterations_per_task` ceilings and the optional
        // `input_queue` restart lives in [`task::run_task`]. See
        // `agent_loop/turn.rs`'s module-level docs for the topology
        // diagram.
        task::run_task(
            self,
            provider,
            executor,
            messages,
            tools,
            event_tx,
            cancellation_token,
            input_queue,
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
    /// `EndTurn` / `StopSequence` terminate the loop unconditionally.
    /// The cook-loop-fix strip (2026-05) removed the dev-loop
    /// `EndTurn` intercept escalation, the per-attempt force-progress
    /// nudges, and the `dev_loop_completion_required` short-circuit
    /// that used to keep the loop spinning until the model produced a
    /// write. The harness now trusts the first `EndTurn` it sees.
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
/// Both maps are invalidated together whenever any successful write
/// tool runs; pulling them out of [`LoopState`] makes that invariant
/// visible at the type level (`update_cache` can take `&mut ToolResultCache`
/// directly) and lets the rest of the loop ignore the cache plumbing.
#[derive(Default)]
pub(crate) struct ToolResultCache {
    /// Exact-key cache: `tool_name + canonical_input_json`.
    pub(crate) exact: HashMap<String, String>,
    /// Secondary, normalized index for `search_code` / `find_files`
    /// that collapses alternation-order and trivial whitespace
    /// variants. Populated alongside `exact` in `update_cache`;
    /// consulted only on a miss of the exact key. Cleared together
    /// with `exact` on any successful write so the "write
    /// invalidates cache" invariant is preserved.
    pub(crate) fuzzy: HashMap<String, String>,
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
}

/// Mutable state carried across iterations of the agent loop.
pub struct LoopState {
    pub(crate) result: AgentLoopResult,
    pub(crate) tool_cache: ToolResultCache,
    pub(crate) exploration_state: ExplorationState,
    pub(crate) budget_state: BudgetState,
    pub(crate) had_any_write: bool,
    /// Set true the first iteration whose tool results contain any
    /// `FileOp` (any successful `write_file` / `edit_file` /
    /// `delete_file`). Cumulative across the run — never reset. Gates the
    /// [`AgentLoopConfig::dev_loop_completion_required`] `EndTurn`
    /// intercept: once a write has happened, `EndTurn` is allowed to
    /// terminate the loop cleanly.
    pub(crate) had_any_file_write: bool,
    /// Set true when `handle_task_done` successfully returns
    /// `stop_loop = true` (i.e. all DoD gates passed). Cumulative
    /// across the run — never reset. Like `had_any_file_write`, this
    /// short-circuits the dev-loop EndTurn intercept so a clean
    /// `task_done` completion is never re-nudged.
    ///
    /// Wired in `tool_execution::check_termination_conditions` by
    /// observing a non-error tool result whose source tool is
    /// `task_done` and whose `stop_loop` flag is set. We deliberately
    /// avoid plumbing `LoopState` into `handle_task_done` itself — the
    /// stop-loop flag is a one-bit handshake that already crosses the
    /// task-executor boundary, so reading it on the loop side keeps
    /// the handler signature small.
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
    /// Per-iteration net file-op accumulator (Phase 1.A). Reset at the
    /// top of every iteration; consulted by Phase 1.B's continuation
    /// runtime to detect "no forward motion this turn".
    pub(crate) turn_diff: turn_diff::TurnDiff,
    /// Phase 1.B: streak tracker for consecutive no-write iterations.
    /// Persists across the loop's iteration boundary so the
    /// continuation runtime can escalate Nudge → Blocked after three
    /// consecutive no-write turns.
    pub(crate) continuation: continuation::ContinuationState,
    /// Phase 1.B: cumulative count of continuation prompts injected
    /// this run. The loop fails the task with `task_blocked` once
    /// this hits [`AgentLoopConfig::max_continuation_turns`].
    pub(crate) total_continuation_turns: u32,
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
            },
            last_context_tokens_estimate: None,
            messages,
            build_baseline: None,
            turn_diff: turn_diff::TurnDiff::default(),
            continuation: continuation::ContinuationState::default(),
            total_continuation_turns: 0,
        }
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

        // One-shot extended-thinking disable flag is re-evaluated each
        // iteration: cleared first, then re-set below for the cases
        // that need it. `build_request` reads the flag to decide
        // whether to clamp `max_tokens` below the auto-thinking
        // threshold. The flag never persists across iterations on
        // its own — every turn either re-arms it or runs with
        // thinking allowed.
        self.thinking.disable_thinking_this_iteration = false;

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

        // Iteration 0 is the explore turn — fast tool calls, not
        // multi-minute deliberation. When the caller opts in via
        // `disable_thinking_iteration_0` (the runner sets this for
        // dev-loop tasks), disable extended thinking for this one
        // iteration so the model emits a tool call quickly instead
        // of "Thought for 2m"-bursting before its first read. The
        // taper logic (and on-truncation restore) continue to work
        // from iteration 1 onward. Chat callers leave the flag off
        // because deliberation on the first turn is often the point.
        if iteration == 0 && config.disable_thinking_iteration_0 {
            self.thinking.disable_thinking_this_iteration = true;
        }

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

    /// Phase 2: dev-loop reasoning-effort policy applied per iteration.
    /// Codex sets `reasoning.effort` explicitly per Responses API call
    /// (codex-rs/core/src/client.rs:698-714); the rules below are the
    /// dev-loop analog tailored to aura's `write_file`/`edit_file`/
    /// `delete_file` surface plus the Phase 1.B continuation runtime.
    ///
    /// Resolution order (first match wins):
    ///
    /// 1. Iteration 0 with `disable_thinking_iteration_0` → `Off`.
    ///    Preserves the existing "fast tool call before first read"
    ///    behaviour the runner opts into for dev-loop tasks.
    /// 2. Iteration 0 otherwise → `Medium` (analysis turn).
    /// 3. `had_any_file_write` → `Low` (forward motion has happened,
    ///    cap the deliberation budget).
    /// 4. `submit_plan_called` → `Low` (the plan exists; codex drops
    ///    to low effort once the agent is committed to an
    ///    implementation phase).
    /// 5. A continuation steering message was just injected
    ///    (`continuation.consecutive_no_write > 0`, Phase 1.B) → `Low`
    ///    (the harness is already pushing forward — don't let the
    ///    model burn 2m of thinking on a re-read).
    /// 6. Otherwise → `Medium`.
    fn compute_thinking_effort(
        &self,
        config: &AgentLoopConfig,
        iteration: usize,
    ) -> ThinkingEffort {
        if iteration == 0 {
            if config.disable_thinking_iteration_0 {
                return ThinkingEffort::Off;
            }
            return ThinkingEffort::Medium;
        }
        if self.had_any_file_write
            || self.submit_plan_called
            || self.continuation.consecutive_no_write > 0
        {
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

        // Phase 2: dev-loop callers opt into the explicit
        // `reasoning.effort` policy. Non-dev-loop (chat / generic)
        // callers stay on the legacy `max_tokens > 2048` auto-enable
        // path by leaving `thinking_effort = None`, so this commit is
        // backwards-compatible for everyone except the dev loop the
        // codex-pattern adoption is targeting.
        let thinking_effort = if config.dev_loop_completion_required {
            Some(self.compute_thinking_effort(config, iteration))
        } else {
            None
        };

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
            ..AgentLoopConfig::default()
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
        let config = AgentLoopConfig::default();
        let state = LoopState::new(&config, vec![Message::user("anything")]);
        let tools = vec![mk_tool("anything_tool")];
        let req = state.build_request(&config, &tools, 1).unwrap();
        assert_eq!(req.tools.len(), 1);
    }

    #[test]
    fn build_request_keeps_tool_hints_scoped_after_first_iteration() {
        let config = AgentLoopConfig {
            tool_hints: Some(vec!["read_file".to_string(), "create_task".to_string()]),
            ..AgentLoopConfig::default()
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
            ..AgentLoopConfig::default()
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
            ..AgentLoopConfig::default()
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
            ..AgentLoopConfig::default()
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
            ..AgentLoopConfig::default()
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
            ..AgentLoopConfig::default()
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
