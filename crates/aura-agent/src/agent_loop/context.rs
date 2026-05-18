//! Context management: compaction, checkpoints, and budget warnings.

use aura_compaction::{
    CompactionAction, CompactionInput, CompactionPolicy, SummaryInput, SummaryOutput,
};
use aura_reasoner::ToolDefinition;
use tokio::sync::mpsc::Sender;

use crate::budget;
use crate::compaction;
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

impl CompactionOutcome {
    #[cfg(test)]
    fn applied_tier(&self) -> Option<compaction::CompactionConfig> {
        match self {
            Self::Applied(tier) => Some(*tier),
            Self::None | Self::NeedsSummary(_) => None,
        }
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

/// Apply proactive compaction when exploration usage is high.
pub(super) fn compact_exploration_if_needed(config: &AgentLoopConfig, state: &mut LoopState) {
    if state.exploration_compaction_done {
        return;
    }
    let threshold = (config.exploration_allowance * 2) / 3;
    if state.exploration_state.count < threshold {
        return;
    }
    if config.max_context_tokens.is_none() {
        return;
    }

    if compaction::compact_exploration_if_needed(
        &mut state.messages,
        state.exploration_state.count,
        config.exploration_allowance,
        config.max_context_tokens,
        state.exploration_compaction_done,
    ) {
        sanitize::validate_and_repair(&mut state.messages);
        state.exploration_compaction_done = true;
    }
}

/// Check and emit budget and exploration warnings.
///
/// In unlimited-iteration mode (`max_iterations == usize::MAX`), the
/// iteration-utilization warnings are skipped — utilization would
/// round to ~0 and the warnings would never fire anyway, but the
/// short-circuit makes the intent explicit and avoids any cast-related
/// precision surprises. Exploration warnings still run because they
/// key off `exploration_allowance`, which is independent of the
/// per-turn iteration cap.
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

    if let Some(warning) = budget::check_exploration_warning(
        &mut state.exploration_state,
        config.exploration_allowance,
    ) {
        helpers::append_warning(&mut state.messages, &warning);
        streaming::emit(event_tx, AgentLoopEvent::Warning(warning));
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
    use crate::compaction::{estimate_message_chars, CompactionConfig};
    use aura_compaction::{
        absolute_byte_tier, pick_stricter_tier, ABSOLUTE_BYTE_AGGRESSIVE_AT,
        ABSOLUTE_BYTE_LIGHT_AT, ABSOLUTE_BYTE_MICRO_AT,
    };
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
            crate::compaction::CompactionConfig::micro(),
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
            crate::compaction::CompactionConfig::aggressive(),
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

    /// Build an alternating user/assistant transcript of `len` messages.
    /// The first message is the cache anchor; the next `middle_count`
    /// messages each carry `middle_chars` of text (and so are candidates
    /// for older-message compaction); the remaining tail are short
    /// "preserve" messages. Roles strictly alternate starting with
    /// `user` so `sanitize::validate_and_repair` doesn't merge or
    /// reorder anything when `compact_if_needed` calls it.
    fn build_transcript(len: usize, middle_count: usize, middle_chars: usize) -> Vec<Message> {
        let mut messages = Vec::with_capacity(len);
        messages.push(Message::user("intro"));
        for i in 0..(len - 1) {
            let role_is_assistant = i % 2 == 0;
            let body = if i < middle_count {
                let ch = if role_is_assistant { 'A' } else { 'B' };
                ch.to_string().repeat(middle_chars)
            } else {
                "tail".to_string()
            };
            if role_is_assistant {
                messages.push(Message::assistant(body));
            } else {
                messages.push(Message::user(body));
            }
        }
        messages
    }

    /// 80 KB of message text with a 200K-token context window resolves
    /// to ~10% utilization — below every threshold in `select_tier`,
    /// so the old % utilization-only trigger would have done nothing.
    /// The absolute-byte arm must still fire and pick the light tier.
    #[test]
    fn absolute_byte_trigger_fires_below_utilization_threshold() {
        let config = AgentLoopConfig {
            max_context_tokens: Some(200_000),
            // Drop max_tokens so `reserved_output_tokens` doesn't push
            // utilization above the lowest `select_tier` threshold
            // (0.15) and force a tier from the % arm.
            max_tokens: 0,
            ..AgentLoopConfig::default()
        };
        // 12 messages: index 0 anchor, indices 1..=3 compactable, last 8 preserved.
        // 3 × 27_000 chars = 81K compactable + a few small tails ≈ 81 KB:
        // above ABSOLUTE_BYTE_LIGHT_AT (64 KB) and below ABSOLUTE_BYTE_AGGRESSIVE_AT.
        let messages = build_transcript(12, 3, 27_000);
        let mut state = LoopState::new(&config, messages);

        let before_chars = estimate_message_chars(&state.messages);
        assert!(
            before_chars >= ABSOLUTE_BYTE_LIGHT_AT,
            "fixture should exceed light threshold; got {before_chars} bytes"
        );

        let chosen = compact_if_needed(&config, &mut state, &[]).applied_tier();

        let light = CompactionConfig::light();
        let tier = chosen.expect("absolute-byte arm should have triggered light tier");
        assert_eq!(tier.tool_result_max_chars, light.tool_result_max_chars);
        assert_eq!(tier.text_max_chars, light.text_max_chars);
        assert_eq!(tier.preserve_recent, light.preserve_recent);

        let after_chars = estimate_message_chars(&state.messages);
        assert!(
            after_chars < before_chars,
            "compaction should have shrunk messages: {before_chars} -> {after_chars}"
        );
    }

    /// When % utilization picks `moderate` but the absolute-byte arm
    /// picks `aggressive`, the stricter pick (aggressive) must win.
    #[test]
    fn absolute_byte_trigger_picks_stricter_tier() {
        // Tight window so 100 KB of chars (~25K tokens) lands in the
        // moderate band (0.60..0.70) for `select_tier`, while still
        // staying in [96 KB, 128 KB) for `absolute_byte_tier` →
        // `aggressive`.
        let config = AgentLoopConfig {
            max_context_tokens: Some(40_000),
            max_tokens: 0,
            ..AgentLoopConfig::default()
        };
        // 12 messages: anchor + 7 large compactable + 4 preserved
        // (aggressive's preserve_recent = 4). 7 × 14_500 ≈ 101.5 KB,
        // which is ≥ 96 KB and < 128 KB.
        let messages = build_transcript(12, 7, 14_500);
        let mut state = LoopState::new(&config, messages);

        let before_chars = estimate_message_chars(&state.messages);
        assert!(
            (ABSOLUTE_BYTE_AGGRESSIVE_AT..ABSOLUTE_BYTE_MICRO_AT).contains(&before_chars),
            "fixture should sit in the absolute-byte aggressive band; got {before_chars}"
        );

        let chosen = compact_if_needed(&config, &mut state, &[])
            .applied_tier()
            .expect("both arms should have triggered a tier");

        let aggressive = CompactionConfig::aggressive();
        assert_eq!(
            chosen.tool_result_max_chars, aggressive.tool_result_max_chars,
            "stricter (aggressive) tier should win over moderate"
        );
        assert_eq!(chosen.text_max_chars, aggressive.text_max_chars);
        assert_eq!(chosen.preserve_recent, aggressive.preserve_recent);
    }

    /// Below the light threshold and below the lowest % utilization
    /// threshold, `compact_if_needed` must leave the transcript alone.
    #[test]
    fn absolute_byte_trigger_no_op_below_light_threshold() {
        let config = AgentLoopConfig {
            max_context_tokens: Some(200_000),
            max_tokens: 0,
            ..AgentLoopConfig::default()
        };
        // ~30 KB total — comfortably under ABSOLUTE_BYTE_LIGHT_AT (64 KB)
        // and well under the 15% utilization floor for select_tier.
        let messages = build_transcript(12, 3, 8_000);
        let mut state = LoopState::new(&config, messages);

        let before_chars = estimate_message_chars(&state.messages);
        assert!(
            before_chars < ABSOLUTE_BYTE_LIGHT_AT,
            "fixture must stay under the light threshold; got {before_chars}"
        );

        let chosen = compact_if_needed(&config, &mut state, &[]).applied_tier();
        assert!(
            chosen.is_none(),
            "neither arm should trigger; got {chosen:?}"
        );

        let after_chars = estimate_message_chars(&state.messages);
        assert_eq!(
            after_chars, before_chars,
            "no compaction should have changed message bytes"
        );
    }

    #[test]
    fn absolute_byte_tier_thresholds_pick_the_right_config() {
        assert!(absolute_byte_tier(0).is_none());
        assert!(absolute_byte_tier(ABSOLUTE_BYTE_LIGHT_AT - 1).is_none());
        assert_eq!(
            absolute_byte_tier(ABSOLUTE_BYTE_LIGHT_AT)
                .unwrap()
                .tool_result_max_chars,
            CompactionConfig::light().tool_result_max_chars
        );
        assert_eq!(
            absolute_byte_tier(ABSOLUTE_BYTE_AGGRESSIVE_AT)
                .unwrap()
                .tool_result_max_chars,
            CompactionConfig::aggressive().tool_result_max_chars
        );
        assert_eq!(
            absolute_byte_tier(ABSOLUTE_BYTE_MICRO_AT)
                .unwrap()
                .tool_result_max_chars,
            CompactionConfig::micro().tool_result_max_chars
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
}
