//! Mutable per-run loop state and its inherent methods.
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split. The struct and its `begin_iteration` / `build_request` /
//! `compute_thinking_effort` / `latest_user_text` helpers live here;
//! the [`super::AgentLoop`] methods that drive a whole run still live
//! in [`super::run`] and [`super::stop_reason`].

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use aura_config::THINKING_AUTO_ENABLE_THRESHOLD;
use aura_model_reasoner::{
    Message, ModelRequest, ModelRequestKind, Role, ThinkingEffort, ToolDefinition,
};

use crate::budget::{BudgetState, ExplorationState};
use crate::types::{AgentLoopResult, BuildBaseline};

use super::cache::ToolResultCache;
use super::config::{parse_cache_retention, AgentLoopConfig};
use super::steering::SteeringRegistry;
use super::{steering, turn_diff};

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
    /// Paths successfully read this session; used by the duplicate-read gate.
    pub(crate) session_read_paths: std::collections::HashSet<PathBuf>,
    /// Per-path read budget granted after a successful write to that path.
    /// Lets the agent inspect changed regions while repairing malformed edits.
    pub(crate) read_after_write_allowances: std::collections::HashMap<PathBuf, u8>,
    /// One-shot latch mirrored from
    /// [`steering::SteeringRegistry::implement_now_injected`]. Kept on
    /// `LoopState` because the pre-dispatch
    /// `tool_pipeline::partition_circling_duplicate_reads` gate
    /// consults it on every batch and rebinding the borrow each call
    /// is needlessly noisy; the field is refreshed in
    /// [`Self::begin_iteration`] right after the registry drains.
    pub(crate) implement_now_injected: bool,
    /// Phase 5: every per-turn steering evaluator installed for this
    /// run. Sources implement [`steering::TurnSteering`] and are
    /// driven by the loop via [`Self::begin_iteration`] (which calls
    /// `begin_turn` + `drain_for_next_turn`) and by
    /// `tool_pipeline::track_tool_effects` (which calls
    /// `observe_tool` on every `(tool, result)` pair).
    pub(crate) steering: SteeringRegistry,
}

impl LoopState {
    pub(super) fn new(config: &AgentLoopConfig, messages: Vec<Message>) -> Self {
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
            session_read_paths: std::collections::HashSet::new(),
            read_after_write_allowances: std::collections::HashMap::new(),
            implement_now_injected: false,
            steering: SteeringRegistry::for_loop(
                config.phase_reset_signal.is_some(),
                config.early_test_oracle.clone(),
            ),
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
    pub(crate) fn begin_iteration(&mut self, config: &AgentLoopConfig, iteration: usize) {
        self.build_cooldown = self.build_cooldown.saturating_sub(1);

        // Phase 1.A: scope the turn-diff to the iteration we are about
        // to execute. The previous iteration's net file ops are no
        // longer relevant — `had_any_file_write` (cumulative latch)
        // and the per-iteration `turn_diff` answer different questions.
        self.turn_diff.reset();

        // Phase 5: drive every installed `TurnSteering` source
        // through one uniform begin-turn → drain pipeline. The
        // registry calls `begin_turn` on every source, then
        // returns the concatenated `drain_for_next_turn` output
        // which the loop routes through the existing `inject`
        // helper. The pre-Phase-5 inline `begin_turn` /
        // `evaluate_implement_now` calls are gone.
        self.steering.begin_turn();
        for kind in self.steering.drain_for_next_turn() {
            steering::inject(&mut self.messages, &kind);
        }
        // Mirror the registry's `implement_now_injected` latch back
        // onto `LoopState` so the pre-dispatch circling-read gate in
        // `tool_pipeline::partition_circling_duplicate_reads`
        // continues to be a synchronous read on `state` rather than
        // taking a fresh borrow on `state.steering` every batch.
        self.implement_now_injected = self.steering.implement_now_injected();

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
    pub(crate) fn compute_thinking_effort(
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

    pub(crate) fn build_request(
        &self,
        config: &AgentLoopConfig,
        tools: &[ToolDefinition],
        iteration: usize,
    ) -> Result<ModelRequest, crate::AgentError> {
        let effective_tools = scope_tools_for_iteration(config, &self.messages, tools);
        let request_kind = pick_request_kind(&effective_tools, config.request_kind, iteration);

        // The cook-loop-fix strip (2026-05) removed the read-only
        // streak counter and the force-tool-choice path that rode on
        // top of it. `tool_choice` is always `Auto`; the model picks
        // its own next move.
        let tool_choice = aura_model_reasoner::ToolChoice::Auto;

        // Disable extended thinking for this one iteration by clamping
        // `max_tokens` below the reasoner's auto-thinking threshold
        // (`> 2048`, see
        // `aura_model_reasoner::anthropic::convert::resolve_thinking`).
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
        //
        // A user-selected thinking level (forwarded from the chat model
        // picker via `AgentLoopConfig::user_thinking_effort`) hard-pins
        // the effort across every iteration, overriding the internal
        // `compute_thinking_effort` taper so the operator's explicit
        // choice always wins. When unset, fall back to the heuristic.
        let thinking_effort = Some(
            config
                .user_thinking_effort
                .unwrap_or_else(|| self.compute_thinking_effort(config, iteration)),
        );

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

/// Narrow `tools` down to domain-relevant entries before the
/// tool-hints logic runs. The classifier is keyed on the most recent
/// pure-text user message, so scratchpad tool-result turns reuse the
/// previous filter rather than widening the surface back to every tool.
fn scope_tools_for_iteration(
    config: &AgentLoopConfig,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Vec<ToolDefinition> {
    let classifier_filtered: Vec<ToolDefinition> = match (
        config.intent_classifier.as_deref(),
        latest_user_text(messages),
    ) {
        (Some(classifier), Some(text)) if !config.intent_classifier_manifest.is_empty() => {
            classifier.filter_tools(text, &config.intent_classifier_manifest, tools)
        }
        _ => tools.to_vec(),
    };

    match &config.tool_hints {
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
    }
}

/// Resolve [`ModelRequestKind`] for the current build_request call.
///
/// Narrow the project-tool override to dev-loop turns only.
///
/// The `ProjectToolTaskExtract` / `ProjectToolSpecGen` request kinds
/// carry a `PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES = 48 KiB` cap in
/// `aura-reasoner::content_profile`. The cap exists so the
/// task-extraction phase of the dev loop can't blow up the model
/// request with arbitrary chat history. The previous wildcard
/// arm — `(true, _, _, _) => ProjectToolTaskExtract` — clobbered
/// any explicit `config.request_kind` (including `Chat`) whenever
/// the task tools happened to be visible. That makes every chat
/// turn for an agent with `create_task`/etc. in scope hard-fail
/// with `EmergencyCapRequired` once history accumulates past
/// ~48 KiB, even though normal chat conversations should be
/// governed by the much-larger chat budget instead.
///
/// Restrict the override to `DevLoopBootstrap`/`Continuation`
/// request kinds, where the task-extraction context invariant
/// actually applies. Plain `Chat` / `Auxiliary` requests now keep
/// their declared `config.request_kind` even when they happen to
/// have task / spec tools available.
fn pick_request_kind(
    effective_tools: &[ToolDefinition],
    request_kind: ModelRequestKind,
    iteration: usize,
) -> ModelRequestKind {
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
    match (has_task_tools, has_spec_tools, request_kind, iteration) {
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
pub(super) fn latest_user_text(messages: &[Message]) -> Option<&str> {
    for msg in messages.iter().rev() {
        if matches!(msg.role, Role::User)
            && msg
                .content
                .iter()
                .any(|b| matches!(b, aura_model_reasoner::ContentBlock::Text { .. }))
        {
            return msg.content.iter().find_map(|b| match b {
                aura_model_reasoner::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            });
        }
    }
    None
}

#[cfg(test)]
mod intent_classifier_tests {
    use super::*;
    use aura_model_reasoner::ToolDefinition;
    use aura_tools::IntentClassifier;
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
                aura_model_reasoner::ToolResultContent::Text("100".into()),
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
                aura_model_reasoner::ToolResultContent::Text("large requirements".into()),
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
        assert!(matches!(
            req.tool_choice,
            aura_model_reasoner::ToolChoice::Auto
        ));
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
        assert!(matches!(
            req.tool_choice,
            aura_model_reasoner::ToolChoice::Auto
        ));
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
