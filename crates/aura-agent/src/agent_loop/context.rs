//! Context management: compaction, checkpoints, and budget warnings.

use aura_compaction::{
    self as compaction, CompactionAction, CompactionInput, CompactionPolicy, SummaryInput,
    SummaryOutput,
};
use aura_reasoner::{ModelRequestKind, ToolDefinition};
use tokio::sync::mpsc::Sender;

use crate::budget;
use crate::dup_audit;
use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::sanitize;
use crate::types::AgentContextBreakdown;
use aura_config::CHARS_PER_TOKEN;

use super::streaming;
use super::{AgentLoopConfig, LoopState};

#[derive(Debug)]
pub(super) enum CompactionOutcome {
    None,
    Applied(compaction::CompactionConfig),
    NeedsSummary(SummaryInput),
}

/// Operator kill switch: when
/// `aura_config::agent().compaction.disabled` is `true` (sourced from
/// `AURA_AGENT_DISABLE_COMPACTION` once at startup), every compaction
/// entry point in the agent loop becomes a no-op. Used to test
/// whether read-spiral behavior is being driven by older `read_file`
/// results getting truncated out of context.
#[must_use]
pub(super) fn compaction_disabled_by_env() -> bool {
    aura_config::agent().compaction.disabled
}

/// Return the [`ModelRequestKind`] the compaction policy should reason
/// about for the upcoming model call at `iteration`.
///
/// [`AgentLoopConfig::request_kind`] is set once at task start (the
/// dev-loop seeds it to [`ModelRequestKind::DevLoopBootstrap`] in
/// `configure_loop_config`) and is never mutated for the life of the
/// task. [`LoopState::build_request`] dynamically swaps the *wire*
/// request kind to [`ModelRequestKind::DevLoopContinuation`] after
/// iteration 0, but the compaction policy reads the stale config field
/// directly. Bootstrap carries a 24 KiB body cap
/// (`DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES`); continuation has none.
/// Without this helper, [`aura_compaction::effective_pressure`] keeps
/// applying the bootstrap cap to every continuation iteration and
/// `cap_pressure` clamps to 1.0 as soon as `state.messages` crosses
/// ~24 KiB — which fires `NeedsSummary` on every iteration regardless
/// of actual context utilisation, doubling the outbound API call rate
/// for the rest of the task.
///
/// The match here mirrors the bootstrap → continuation transition
/// [`LoopState::build_request`] already performs (see the
/// `(_, _, DevLoopBootstrap, _) => DevLoopContinuation` arm) so the
/// compaction policy sees the same kind the next wire request will
/// carry. Non-dev-loop kinds (`Chat`, explicit `Auxiliary`, the
/// project-tool kinds) pass through unchanged because they either
/// have no body cap or already carry the correct kind statically.
#[must_use]
pub(super) fn effective_compaction_request_kind(
    config: &AgentLoopConfig,
    iteration: usize,
) -> ModelRequestKind {
    match (config.request_kind, iteration) {
        (ModelRequestKind::DevLoopBootstrap, 0) => ModelRequestKind::DevLoopBootstrap,
        (ModelRequestKind::DevLoopBootstrap, _) => ModelRequestKind::DevLoopContinuation,
        (kind, _) => kind,
    }
}

fn reserved_output_tokens(config: &AgentLoopConfig, max_ctx: u64) -> u64 {
    u64::from(config.max_tokens).min(max_ctx)
}

#[cfg(test)]
fn compaction_pressure_tokens(
    config: &AgentLoopConfig,
    estimated_tokens: u64,
    max_ctx: u64,
) -> u64 {
    estimated_tokens
        .saturating_add(reserved_output_tokens(config, max_ctx))
        .min(max_ctx)
}

fn heuristic_context_tokens(messages: &[aura_reasoner::Message]) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (compaction::estimate_message_chars(messages) / CHARS_PER_TOKEN) as u64
    }
}

fn current_context_tokens(state: &LoopState) -> u64 {
    state
        .last_context_tokens_estimate
        .unwrap_or_default()
        .max(heuristic_context_tokens(&state.messages))
}

/// Char-to-token conversion shared by every per-bucket estimate. Wraps
/// the existing `chars / CHARS_PER_TOKEN` heuristic so the breakdown
/// stays directly comparable to [`AgentLoopResult::estimated_context_tokens`].
fn chars_to_tokens(chars: usize) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    {
        (chars / CHARS_PER_TOKEN) as u64
    }
}

/// Recompute every per-bucket token estimate from the current loop
/// state. Called after every compaction step (and after overflow
/// recovery) so the value stays in sync with `estimated_context_tokens`.
///
/// Callers pass the *effective* tool surface (post intent-classifier /
/// tool-hints filtering) when available, so the bucket reflects what
/// the model actually receives on the next turn rather than the raw
/// configured surface.
///
/// `system_prompt_tokens` is reported net of [`AgentLoopConfig::skills_chars`]
/// because the runtime injects skill summaries directly into the
/// system prompt; without the subtraction those chars would be
/// double-counted (once under "System prompt" and once under
/// "Skills") and the stacked-bar breakdown in the UI would always
/// look fuller than `estimated_context_tokens`.
fn recompute_breakdown(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    effective_tools: &[ToolDefinition],
) {
    let system_prompt_chars = config
        .system_prompt
        .len()
        .saturating_sub(config.skills_chars);
    state.result.context_breakdown = AgentContextBreakdown {
        system_prompt_tokens: chars_to_tokens(system_prompt_chars),
        tools_tokens: chars_to_tokens(compaction::tools_chars(effective_tools)),
        skills_tokens: chars_to_tokens(config.skills_chars),
        mcp_tokens: 0,
        subagents_tokens: chars_to_tokens(config.subagents_chars),
        conversation_tokens: heuristic_context_tokens(&state.messages),
        // Cache hit/miss numbers come from `accumulate_response`
        // after each model reply; `recompute_breakdown` runs
        // pre-call and has no usage data yet, so default to 0.
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    };
}

/// Sanitize messages and apply compaction if context utilization is high.
///
/// Returns the compaction outcome so the async loop can perform model-backed
/// summary escalation outside the pure compaction crate.
///
/// `iteration` is the 0-based index of the upcoming model call.
/// [`effective_compaction_request_kind`] uses it to project the stale
/// [`AgentLoopConfig::request_kind`] forward through the bootstrap →
/// continuation transition so the compaction policy doesn't keep
/// applying the bootstrap body cap to every post-bootstrap turn.
#[allow(clippy::cast_precision_loss)]
pub(super) fn compact_if_needed(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    tools: &[ToolDefinition],
    iteration: usize,
) -> CompactionOutcome {
    sanitize::validate_and_repair(&mut state.messages);

    if compaction_disabled_by_env() {
        recompute_breakdown(config, state, tools);
        return CompactionOutcome::None;
    }

    let Some(max_ctx) = config.max_context_tokens else {
        recompute_breakdown(config, state, tools);
        return CompactionOutcome::None;
    };

    let estimated_tokens = current_context_tokens(state);
    state.result.estimated_context_tokens = estimated_tokens;
    let reserved_tokens = reserved_output_tokens(config, max_ctx);
    let raw_message_bytes = compaction::estimate_message_chars(&state.messages);
    let request_kind = effective_compaction_request_kind(config, iteration);

    // Phase 8: fire `PreCompact` hook. A `Block` decision skips
    // compaction this turn. Empty-engine short-circuit guarantees
    // zero overhead for empty installs.
    if let Some(host) = config.plugin_hooks.as_ref() {
        if !host.is_empty(aura_plugin_hooks::HookEvent::PreCompact) {
            let outcome = host.fire_pre_compact(estimated_tokens, "auto");
            if outcome.is_blocked() {
                tracing::info!("PreCompact hook blocked compaction this turn");
                recompute_breakdown(config, state, tools);
                return CompactionOutcome::None;
            }
        }
    }

    let report = compaction::compact_messages(CompactionInput {
        messages: &mut state.messages,
        policy: CompactionPolicy {
            current_context_tokens: Some(estimated_tokens),
            raw_message_bytes: Some(raw_message_bytes),
            request_kind: Some(request_kind),
            ..CompactionPolicy::new(Some(max_ctx), estimated_tokens, reserved_tokens)
        },
    });
    let outcome = match report.action {
        CompactionAction::Applied(tier) => CompactionOutcome::Applied(tier),
        CompactionAction::NeedsSummary(input) => CompactionOutcome::NeedsSummary(input),
        CompactionAction::None => CompactionOutcome::None,
    };

    if !matches!(outcome, CompactionOutcome::None) {
        dup_audit::audit_tool_result_duplicates(&state.messages, "compact_if_needed.post_splice");
        sanitize::validate_and_repair(&mut state.messages);
        let compacted_tokens = heuristic_context_tokens(&state.messages);
        state.last_context_tokens_estimate = Some(compacted_tokens);
        state.result.estimated_context_tokens = compacted_tokens;

        // Phase 8: fire `PostCompact` hook (observer-only).
        if let Some(host) = config.plugin_hooks.as_ref() {
            if !host.is_empty(aura_plugin_hooks::HookEvent::PostCompact) {
                let summary = match &outcome {
                    CompactionOutcome::Applied(tier) => format!("{tier:?}"),
                    CompactionOutcome::NeedsSummary(_) => "needs_summary".to_string(),
                    CompactionOutcome::None => "none".to_string(),
                };
                host.fire_post_compact(estimated_tokens, compacted_tokens, &summary);
            }
        }
    }

    recompute_breakdown(config, state, tools);
    outcome
}

pub(super) fn apply_summary_output(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    tools: &[ToolDefinition],
    summary: SummaryOutput,
) -> bool {
    if compaction_disabled_by_env() {
        recompute_breakdown(config, state, tools);
        return false;
    }
    let report = compaction::Compactor::new().apply_summary(&mut state.messages, summary);
    if !report.reduced() {
        recompute_breakdown(config, state, tools);
        return false;
    }

    dup_audit::audit_tool_result_duplicates(&state.messages, "apply_summary_output.post_splice");
    sanitize::validate_and_repair(&mut state.messages);
    let compacted_tokens = heuristic_context_tokens(&state.messages);
    state.last_context_tokens_estimate = Some(compacted_tokens);
    state.result.estimated_context_tokens = compacted_tokens;
    recompute_breakdown(config, state, tools);
    true
}

/// Apply a specific compaction tier after a provider rejects the request for
/// being too large. Returns `true` when the prompt was actually reduced.
///
/// Phase 7 dropped the only production caller
/// ([`super::AgentLoop::retry_after_context_overflow`], itself a
/// `BufferedTransport`-only ladder). The helper is retained so a
/// future caller — most likely an inline overflow-recovery path
/// added to the pump driver — can reuse the tier mechanics without
/// re-deriving the chars/tokens accounting.
#[allow(dead_code)]
pub(super) fn compact_for_overflow(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    tier: compaction::CompactionConfig,
    tools: &[ToolDefinition],
) -> bool {
    if compaction_disabled_by_env() {
        recompute_breakdown(config, state, tools);
        return false;
    }
    sanitize::validate_and_repair(&mut state.messages);
    let before_chars = compaction::estimate_message_chars(&state.messages);
    let before_tokens = current_context_tokens(state);

    let report = compaction::recover_overflow(&mut state.messages, tier);
    dup_audit::audit_tool_result_duplicates(&state.messages, "compact_for_overflow.post_splice");
    sanitize::validate_and_repair(&mut state.messages);

    let after_chars = report.after_chars;
    let after_tokens = heuristic_context_tokens(&state.messages);
    state.last_context_tokens_estimate = Some(after_tokens);
    state.result.estimated_context_tokens = after_tokens;

    recompute_breakdown(config, state, tools);

    after_chars < before_chars || after_tokens < before_tokens
}

/// Emit the first-write checkpoint warning once.
pub(super) fn emit_checkpoint_if_needed(
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut LoopState,
) {
    if !state.had_any_write || state.checkpoint_emitted {
        return;
    }
    state.checkpoint_emitted = true;
    let msg = "NOTE: You've made your first file change. Before making more changes, \
               consider verifying your work (e.g., run the build or tests) to catch \
               issues early."
        .to_string();
    helpers::append_warning(&mut state.messages, &msg);
    streaming::emit(event_tx, AgentLoopEvent::Warning(msg));
}

/// Check and emit budget warnings.
///
/// In unlimited-iteration mode (`max_iterations == usize::MAX`), the
/// iteration-utilization warnings are skipped — utilization would
/// round to ~0 and the warnings would never fire anyway, but the
/// short-circuit makes the intent explicit and avoids any cast-related
/// precision surprises. The exploration "approaching limit" warning
/// was removed by the cook-loop-fix strip (2026-05) along with the
/// hard exploration cap.
#[allow(clippy::cast_precision_loss)]
pub(super) fn check_budget_warnings(
    config: &AgentLoopConfig,
    event_tx: Option<&Sender<AgentLoopEvent>>,
    state: &mut LoopState,
    iteration: usize,
) {
    if config.max_iterations != usize::MAX {
        let utilization = (iteration + 1) as f64 / config.max_iterations as f64;
        if let Some(warning) =
            budget::check_budget_warning(&mut state.budget_state, utilization, state.had_any_write)
        {
            helpers::append_warning(&mut state.messages, &warning);
            streaming::emit(event_tx, AgentLoopEvent::Warning(warning));
        }
    }
}

/// Check whether the loop should stop due to budget exhaustion.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub(super) fn should_stop_for_budget(
    config: &AgentLoopConfig,
    state: &LoopState,
    iteration: usize,
) -> bool {
    let total_tokens = state.result.total_input_tokens + state.result.total_output_tokens;
    let iterations_done = (iteration as u64) + 1;
    let avg_tokens = total_tokens / iterations_done.max(1);
    budget::should_stop_for_budget(
        iteration + 1,
        config.max_iterations,
        avg_tokens,
        total_tokens,
        config.credit_budget,
    )
}

#[cfg(test)]
mod tests {
    use super::{
        compact_for_overflow, compact_if_needed, compaction_pressure_tokens,
        effective_compaction_request_kind, heuristic_context_tokens, reserved_output_tokens,
        CompactionOutcome,
    };
    use crate::agent_loop::AgentLoopConfig;
    use crate::agent_loop::LoopState;
    use aura_compaction::{pick_stricter_tier, CompactionConfig};
    use aura_reasoner::{Message, ModelRequestKind, ToolDefinition};
    use std::sync::{Mutex, OnceLock};

    /// Serialize tests that swap `aura_config` to flip the compaction
    /// kill switch. Sibling tests that call `compact_if_needed` /
    /// `compact_for_overflow` read the same config, so any toggle has
    /// to be coordinated to keep parallel-test runs deterministic.
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn install_compaction_disabled(disabled: bool) -> aura_config::ConfigGuard {
        let mut cfg = aura_config::current();
        cfg.agent.compaction.disabled = disabled;
        aura_config::install_for_test(cfg)
    }

    fn dummy_tool(name: &str, description: &str) -> ToolDefinition {
        ToolDefinition::new(
            name,
            description,
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
        )
    }

    #[test]
    fn reserves_max_tokens_for_output_headroom() {
        let config = AgentLoopConfig {
            max_tokens: 16_384,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        assert_eq!(reserved_output_tokens(&config, 200_000), 16_384);
    }

    #[test]
    fn reserve_is_capped_by_context_window() {
        let config = AgentLoopConfig {
            max_tokens: 16_384,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        assert_eq!(reserved_output_tokens(&config, 8_000), 8_000);
    }

    #[test]
    fn pressure_tokens_include_output_reserve() {
        let config = AgentLoopConfig {
            max_tokens: 20_000,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        assert_eq!(compaction_pressure_tokens(&config, 60_000, 100_000), 80_000);
    }

    #[test]
    fn overflow_compaction_reports_progress_when_history_shrinks() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new(
            &config,
            vec![
                Message::user("intro"),
                Message::assistant("A".repeat(4_000)),
                Message::user("B".repeat(4_000)),
                Message::assistant("C".repeat(4_000)),
                Message::user("latest"),
            ],
        );
        state.last_context_tokens_estimate = Some(heuristic_context_tokens(&state.messages));

        assert!(compact_for_overflow(
            &config,
            &mut state,
            aura_compaction::CompactionConfig::micro(),
            &[],
        ));
    }

    #[test]
    fn overflow_compaction_reports_no_progress_when_nothing_can_change() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new(&config, vec![Message::user("hello")]);
        state.last_context_tokens_estimate = Some(heuristic_context_tokens(&state.messages));

        assert!(!compact_for_overflow(
            &config,
            &mut state,
            aura_compaction::CompactionConfig::aggressive(),
            &[],
        ));
    }

    /// `compact_if_needed` is the single place that recomputes the
    /// per-bucket breakdown each turn. Verify every bucket lights up
    /// from the obvious sources and that `mcp_tokens` stays at 0
    /// (reserved for future MCP support).
    #[test]
    fn compact_if_needed_populates_context_breakdown() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let config = AgentLoopConfig {
            // Long enough that chars/CHARS_PER_TOKEN rounds to >= 1
            // even after `recompute_breakdown` subtracts `skills_chars`.
            system_prompt: "S".repeat(200),
            // 80 chars / 4 chars-per-token = 20 tokens.
            skills_chars: 80,
            // 60 chars / 4 = 15 tokens.
            subagents_chars: 60,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let mut state = LoopState::new(
            &config,
            vec![Message::user("hello"), Message::assistant("M".repeat(200))],
        );
        let tools = vec![
            dummy_tool("read_file", "Read a file from disk."),
            dummy_tool("write_file", "Write a file to disk."),
        ];

        compact_if_needed(&config, &mut state, &tools, 0);

        let breakdown = &state.result.context_breakdown;
        // system_prompt is reported net of `skills_chars` to avoid
        // double-counting injected skill text. (200 - 80) / 4 = 30.
        assert_eq!(breakdown.system_prompt_tokens, 30);
        assert!(
            breakdown.tools_tokens > 0,
            "tools bucket should be > 0 with two tool defs"
        );
        assert_eq!(breakdown.skills_tokens, 20);
        assert_eq!(breakdown.subagents_tokens, 15);
        assert_eq!(breakdown.mcp_tokens, 0, "mcp bucket is reserved (no MCP)");
        assert!(
            breakdown.conversation_tokens > 0,
            "conversation bucket should reflect the live transcript"
        );
    }

    /// Empty inputs everywhere should yield a near-zero breakdown.
    /// `validate_and_repair` always inserts a sentinel user message
    /// when the conversation is empty, so `conversation_tokens` is
    /// allowed to be small but non-zero — the static buckets must
    /// stay at zero so the frontend's "all-zero ⇒ unavailable"
    /// sentinel still triggers correctly.
    #[test]
    fn compact_if_needed_static_buckets_zero_when_nothing_configured() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let config = AgentLoopConfig::for_agent("claude-test-model");
        let mut state = LoopState::new(&config, vec![]);

        compact_if_needed(&config, &mut state, &[], 0);

        let b = &state.result.context_breakdown;
        assert_eq!(b.system_prompt_tokens, 0);
        assert_eq!(b.tools_tokens, 0);
        assert_eq!(b.skills_tokens, 0);
        assert_eq!(b.subagents_tokens, 0);
        assert_eq!(b.mcp_tokens, 0);
    }

    /// Setting `aura_config::agent().compaction.disabled = true`
    /// (sourced from `AURA_AGENT_DISABLE_COMPACTION`) must
    /// short-circuit `compact_if_needed` so older `read_file` results
    /// never get truncated, even on a config that would normally hit
    /// the aggressive tier. This is the operator escape hatch used to
    /// test whether read-spiral behavior is being driven by mid-run
    /// truncation.
    #[test]
    fn kill_switch_disables_compact_if_needed() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let _cfg = install_compaction_disabled(true);

        let huge = "X".repeat(200_000);
        let mut messages = vec![
            Message::user("start"),
            Message::assistant(huge.clone()),
            Message::user("more"),
            Message::assistant(huge.clone()),
        ];
        let config = AgentLoopConfig {
            max_context_tokens: Some(8_000),
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let before_chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
        let mut state = LoopState::new(&config, std::mem::take(&mut messages));

        let outcome = compact_if_needed(&config, &mut state, &[], 0);

        let after_chars: usize = state.messages.iter().map(|m| m.text_content().len()).sum();

        assert!(
            matches!(outcome, super::CompactionOutcome::None),
            "kill switch must report None outcome"
        );
        assert_eq!(
            after_chars, before_chars,
            "kill switch must leave message bytes untouched"
        );
    }

    #[test]
    fn pick_stricter_tier_prefers_smaller_tool_result_cap() {
        let light = CompactionConfig::light();
        let aggressive = CompactionConfig::aggressive();

        assert_eq!(
            pick_stricter_tier(Some(light), Some(aggressive))
                .unwrap()
                .tool_result_max_chars,
            aggressive.tool_result_max_chars
        );
        assert_eq!(
            pick_stricter_tier(Some(aggressive), Some(light))
                .unwrap()
                .tool_result_max_chars,
            aggressive.tool_result_max_chars
        );
        assert_eq!(
            pick_stricter_tier(Some(light), None)
                .unwrap()
                .tool_result_max_chars,
            light.tool_result_max_chars
        );
        assert_eq!(
            pick_stricter_tier(None, Some(aggressive))
                .unwrap()
                .tool_result_max_chars,
            aggressive.tool_result_max_chars
        );
        assert!(pick_stricter_tier(None, None).is_none());
    }

    // -----------------------------------------------------------------
    // effective_compaction_request_kind: pins the bootstrap →
    // continuation projection so `compact_if_needed` / `accumulate_response`
    // see the same kind that `build_request` will actually ship on the
    // wire. Without this projection the dev-loop bootstrap's 24 KiB body
    // cap leaks into every continuation iteration and pins compaction
    // cap-pressure at 1.0 for the rest of the task (which fires
    // `NeedsSummary` on every iteration and doubles the outbound
    // Anthropic call rate).
    // -----------------------------------------------------------------

    #[test]
    fn effective_kind_keeps_bootstrap_on_iter_zero() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::DevLoopBootstrap,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        assert_eq!(
            effective_compaction_request_kind(&config, 0),
            ModelRequestKind::DevLoopBootstrap
        );
    }

    #[test]
    fn effective_kind_projects_bootstrap_to_continuation_after_iter_zero() {
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::DevLoopBootstrap,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        for iter in 1usize..=8 {
            assert_eq!(
                effective_compaction_request_kind(&config, iter),
                ModelRequestKind::DevLoopContinuation,
                "iteration {iter} should project bootstrap → continuation"
            );
        }
    }

    #[test]
    fn effective_kind_passes_other_kinds_through_unchanged() {
        for kind in [
            ModelRequestKind::Chat,
            ModelRequestKind::DevLoopContinuation,
            ModelRequestKind::Auxiliary,
            ModelRequestKind::ProjectToolSpecGen,
            ModelRequestKind::ProjectToolTaskExtract,
        ] {
            let config = AgentLoopConfig {
                request_kind: kind,
                ..AgentLoopConfig::for_agent("claude-test-model")
            };
            for iter in 0usize..=3 {
                assert_eq!(
                    effective_compaction_request_kind(&config, iter),
                    kind,
                    "non-dev-loop kinds must pass through unchanged ({kind:?} @ iter {iter})"
                );
            }
        }
    }

    /// Regression for the doubled-Anthropic-call-rate bug: in a
    /// dev-loop run with the bootstrap kind seeded into the config,
    /// passing `iteration > 0` to `compact_if_needed` must NOT escalate
    /// to `NeedsSummary` on a transcript that only crosses the 24 KiB
    /// bootstrap body cap (and is otherwise well below
    /// `summary_at = 0.85` on the model's real context window).
    ///
    /// Before the fix, the stale `config.request_kind = DevLoopBootstrap`
    /// flowed into `CompactionPolicy::request_kind` for every iteration;
    /// the resulting `cap_pressure = raw_message_bytes / 24576` clamped
    /// to 1.0 the moment the transcript crossed 24 KiB and fired
    /// `NeedsSummary` on every subsequent call.
    #[test]
    fn compact_if_needed_does_not_escalate_when_continuation_outgrows_bootstrap_cap() {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());

        // Real opus-class context window so context-pressure stays well
        // below the 0.85 summary threshold; the only signal that could
        // possibly trigger NeedsSummary here is the bootstrap body-cap
        // leak. ~40 KiB of transcript clears the 24 KiB bootstrap cap
        // with margin but is < 0.01% of the 1 M-token context window.
        let config = AgentLoopConfig {
            request_kind: ModelRequestKind::DevLoopBootstrap,
            max_context_tokens: Some(1_000_000),
            max_tokens: 16_384,
            ..AgentLoopConfig::for_agent("claude-test-model")
        };
        let big_payload = "X".repeat(40_000);
        let mut state = LoopState::new(
            &config,
            vec![
                Message::user("seed"),
                Message::assistant(big_payload),
                Message::user("next"),
            ],
        );
        state.last_context_tokens_estimate = Some(heuristic_context_tokens(&state.messages));

        // iteration > 0 ⇒ effective kind = DevLoopContinuation (no cap).
        let outcome = compact_if_needed(&config, &mut state, &[], 5);

        assert!(
            matches!(outcome, CompactionOutcome::None | CompactionOutcome::Applied(_)),
            "continuation iteration on a real-sized context window must not fire NeedsSummary; got {outcome:?}"
        );
    }

    // (The "bootstrap iteration 0 must escalate" mirror test was
    // dropped because the assertion was too coupled to internal
    // compaction-tier byte budgets: a single 40 KiB assistant blob is
    // compressible enough that the local `Aggressive` tier already
    // brings the transcript under `target_total_chars`, so
    // `NeedsSummary` legitimately doesn't fire even though the cap
    // pressure was correctly observed. The bootstrap-side contract is
    // pinned at the helper level by
    // `effective_kind_keeps_bootstrap_on_iter_zero` above, which is
    // the only branch this fix actually changes.)
}
