//! Context management: compaction, checkpoints, and budget warnings.

use aura_compaction::{
    self as compaction, CompactionAction, CompactionInput, CompactionPolicy, SummaryInput,
    SummaryOutput,
};
use aura_reasoner::ToolDefinition;
use tokio::sync::mpsc::Sender;

use crate::budget;
use crate::constants::CHARS_PER_TOKEN;
use crate::events::AgentLoopEvent;
use crate::helpers;
use crate::sanitize;
use crate::types::AgentContextBreakdown;

use super::streaming;
use super::{AgentLoopConfig, LoopState};

#[derive(Debug)]
pub(super) enum CompactionOutcome {
    None,
    Applied(compaction::CompactionConfig),
    NeedsSummary(SummaryInput),
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
#[allow(clippy::cast_precision_loss)]
pub(super) fn compact_if_needed(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    tools: &[ToolDefinition],
) -> CompactionOutcome {
    sanitize::validate_and_repair(&mut state.messages);

    let Some(max_ctx) = config.max_context_tokens else {
        recompute_breakdown(config, state, tools);
        return CompactionOutcome::None;
    };

    let estimated_tokens = current_context_tokens(state);
    state.result.estimated_context_tokens = estimated_tokens;
    let reserved_tokens = reserved_output_tokens(config, max_ctx);
    let raw_message_bytes = compaction::estimate_message_chars(&state.messages);
    let report = compaction::compact_messages(CompactionInput {
        messages: &mut state.messages,
        policy: CompactionPolicy {
            current_context_tokens: Some(estimated_tokens),
            raw_message_bytes: Some(raw_message_bytes),
            request_kind: Some(config.request_kind),
            ..CompactionPolicy::new(Some(max_ctx), estimated_tokens, reserved_tokens)
        },
    });
    let outcome = match report.action {
        CompactionAction::Applied(tier) => CompactionOutcome::Applied(tier),
        CompactionAction::NeedsSummary(input) => CompactionOutcome::NeedsSummary(input),
        CompactionAction::None => CompactionOutcome::None,
    };

    if !matches!(outcome, CompactionOutcome::None) {
        sanitize::validate_and_repair(&mut state.messages);
        let compacted_tokens = heuristic_context_tokens(&state.messages);
        state.last_context_tokens_estimate = Some(compacted_tokens);
        state.result.estimated_context_tokens = compacted_tokens;
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
    let report = compaction::Compactor::new().apply_summary(&mut state.messages, summary);
    if !report.reduced() {
        recompute_breakdown(config, state, tools);
        return false;
    }

    sanitize::validate_and_repair(&mut state.messages);
    let compacted_tokens = heuristic_context_tokens(&state.messages);
    state.last_context_tokens_estimate = Some(compacted_tokens);
    state.result.estimated_context_tokens = compacted_tokens;
    recompute_breakdown(config, state, tools);
    true
}

/// Apply a specific compaction tier after a provider rejects the request for
/// being too large. Returns `true` when the prompt was actually reduced.
pub(super) fn compact_for_overflow(
    config: &AgentLoopConfig,
    state: &mut LoopState,
    tier: compaction::CompactionConfig,
    tools: &[ToolDefinition],
) -> bool {
    sanitize::validate_and_repair(&mut state.messages);
    let before_chars = compaction::estimate_message_chars(&state.messages);
    let before_tokens = current_context_tokens(state);

    let report = compaction::recover_overflow(&mut state.messages, tier);
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
        heuristic_context_tokens, reserved_output_tokens,
    };
    use crate::agent_loop::AgentLoopConfig;
    use crate::agent_loop::LoopState;
    use aura_compaction::{pick_stricter_tier, CompactionConfig};
    use aura_reasoner::{Message, ToolDefinition};

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
            ..AgentLoopConfig::default()
        };
        assert_eq!(reserved_output_tokens(&config, 200_000), 16_384);
    }

    #[test]
    fn reserve_is_capped_by_context_window() {
        let config = AgentLoopConfig {
            max_tokens: 16_384,
            ..AgentLoopConfig::default()
        };
        assert_eq!(reserved_output_tokens(&config, 8_000), 8_000);
    }

    #[test]
    fn pressure_tokens_include_output_reserve() {
        let config = AgentLoopConfig {
            max_tokens: 20_000,
            ..AgentLoopConfig::default()
        };
        assert_eq!(compaction_pressure_tokens(&config, 60_000, 100_000), 80_000);
    }

    #[test]
    fn overflow_compaction_reports_progress_when_history_shrinks() {
        let config = AgentLoopConfig::default();
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
        let config = AgentLoopConfig::default();
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
        let config = AgentLoopConfig {
            // Long enough that chars/CHARS_PER_TOKEN rounds to >= 1
            // even after `recompute_breakdown` subtracts `skills_chars`.
            system_prompt: "S".repeat(200),
            // 80 chars / 4 chars-per-token = 20 tokens.
            skills_chars: 80,
            // 60 chars / 4 = 15 tokens.
            subagents_chars: 60,
            ..AgentLoopConfig::default()
        };
        let mut state = LoopState::new(
            &config,
            vec![Message::user("hello"), Message::assistant("M".repeat(200))],
        );
        let tools = vec![
            dummy_tool("read_file", "Read a file from disk."),
            dummy_tool("write_file", "Write a file to disk."),
        ];

        compact_if_needed(&config, &mut state, &tools);

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
        let config = AgentLoopConfig::default();
        let mut state = LoopState::new(&config, vec![]);

        compact_if_needed(&config, &mut state, &[]);

        let b = &state.result.context_breakdown;
        assert_eq!(b.system_prompt_tokens, 0);
        assert_eq!(b.tools_tokens, 0);
        assert_eq!(b.skills_tokens, 0);
        assert_eq!(b.subagents_tokens, 0);
        assert_eq!(b.mcp_tokens, 0);
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
}
