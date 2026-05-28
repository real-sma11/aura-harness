//! [`AgentLoopConfig`] and its [`AgentLoopConfig::for_agent`]
//! constructor.
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split. Field grouping into `TimeoutConfig` / `BillingConfig` /
//! `SteeringConfig` / `CompactionConfig` sub-structs is intentionally
//! deferred to Phase 8: the regrouping would change every call site
//! that constructs an `AgentLoopConfig` with `..for_agent(model)`
//! syntax, and the public-API churn is not worth bundling with the
//! file-size split. The flat struct is preserved as-is so Phase 3 is
//! a pure restructure.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use aura_config::{
    EarlyTestOracleConfig, AUTO_BUILD_COOLDOWN, MAX_ITERATIONS, THINKING_MIN_BUDGET,
    THINKING_TAPER_AFTER, THINKING_TAPER_FACTOR,
};
use aura_reasoner::{ModelRequestKind, ToolDefinition};
use aura_tools::IntentClassifier;

use crate::types::TurnObserver;

/// Configuration for the agent loop.
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// Maximum iterations (model calls) per turn.
    ///
    /// Defaults to [`aura_config::MAX_ITERATIONS`], which is
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
    /// Initial response-token budget seeded into
    /// `LoopState::thinking` at the start of a turn. When `Some`,
    /// this overrides the default of `max_tokens` and lets the
    /// runner cap the loop's *starting* budget below `max_tokens`
    /// (e.g. derived from the model card's `max_thinking_tokens`)
    /// while still allowing the on-truncation restore path to lift
    /// back to the full `max_tokens` ceiling. When `None`, the loop
    /// falls back to `max_tokens` (preserving the pre-Phase-6
    /// behavior).
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
    /// Populated via [`aura_protocol::AgentCapabilities::intent_classifier`]
    /// on the [`aura_protocol::RuntimeRequest`] submitted to `POST /v1/run`.
    /// This lets the harness reproduce the aura-os CEO super-agent's
    /// tier-1/tier-2 filtering without baking the tool manifest into
    /// the harness binary.
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
    /// `Arc<AtomicBool>` flips to `true`, `LoopState::begin_iteration`
    /// resets the exploration counter so the implementation phase has
    /// a fresh budget. `None` for non-task callers (e.g. chat).
    pub phase_reset_signal: Option<Arc<AtomicBool>>,
    /// Dev-loop signal: pin reasoning effort to
    /// [`aura_reasoner::ThinkingEffort::Medium`] across every
    /// iteration of the run (see
    /// `LoopState::compute_thinking_effort`).
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
    /// flag now only feeds `LoopState::compute_thinking_effort`.
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
    /// [`AgentError::IterationBudgetExceeded`](crate::AgentError::IterationBudgetExceeded).
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
    /// Per-task config for the early test-gate oracle (Phase 3a of
    /// the reread-efficiency plan, wired in Phase 5 of the core-loop
    /// architecture refactor).
    ///
    /// `None` keeps the oracle off; `Some(EarlyTestOracleConfig {
    /// enabled: true, test_command: Some(_) })` installs the
    /// [`crate::agent_loop::steering::EarlyTestOracle`] source into
    /// the per-run [`crate::agent_loop::steering::SteeringRegistry`].
    ///
    /// `TaskRun` automatons populate this from their dispatch JSON's
    /// `early_test_oracle: bool` field via
    /// [`crate::agent_runner::AgentRunnerConfig::early_test_oracle`].
    /// Chat / generic callers leave it `None`.
    pub early_test_oracle: Option<EarlyTestOracleConfig>,
    /// **Phase 8** plugin hook host. When `Some`, the agent loop
    /// fires lifecycle events through the host's `fire_*`
    /// helpers; when `None`, the loop carries zero hook overhead.
    ///
    /// The host's `is_empty(event)` short-circuit guarantees zero
    /// overhead even when the host is `Some` but no hooks are
    /// registered for a given event — the empty-install backward-
    /// compat invariant.
    pub plugin_hooks: Option<aura_plugin_hooks::PluginHookHost>,
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
            // Off by default — chat / generic callers do not declare
            // a project test command. The dev-loop / TaskRun path
            // populates `Some(EarlyTestOracleConfig { enabled: true,
            // test_command: ... })` from its dispatch JSON.
            early_test_oracle: None,
            // Phase 8: plugin hook firing is opt-in. The runtime
            // crate populates this when a plugin runtime is
            // materialised at session start; chat / unit-test
            // callers leave it `None`.
            plugin_hooks: None,
        }
    }
}

/// Parse the wire-level `prompt_cache_retention` string forwarded by
/// `aura-os` into a typed [`aura_reasoner::PromptCacheRetention`].
/// Unknown or blank values fall through as `None` so the reasoner
/// falls back to the upstream provider default.
///
/// Lives next to [`AgentLoopConfig`] because every call site is a
/// `ModelRequest::builder(...).prompt_cache_retention(...)` step on
/// the config's own `prompt_cache_retention` field; co-locating
/// keeps the wire-string contract one file away from the consumers.
pub(super) fn parse_cache_retention(
    value: Option<&str>,
) -> Option<aura_reasoner::PromptCacheRetention> {
    let v = value?.trim();
    match v {
        "24h" | "h24" | "hours_24" | "Hours24" => {
            Some(aura_reasoner::PromptCacheRetention::Hours24)
        }
        "in_memory" | "InMemory" | "memory" => Some(aura_reasoner::PromptCacheRetention::InMemory),
        _ => None,
    }
}
