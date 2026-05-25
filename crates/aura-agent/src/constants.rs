//! Product constants and numeric parameters for the agent layer.

// ---------------------------------------------------------------------------
// Default model identifiers
// ---------------------------------------------------------------------------

/// Default frontier model for agent loops and sessions.
pub const DEFAULT_MODEL: &str = "claude-opus-4-6";

/// Fallback model used when the primary model is unavailable.
pub const FALLBACK_MODEL: &str = "claude-sonnet-4-6";

// ---------------------------------------------------------------------------
// Tool result caching
// ---------------------------------------------------------------------------

/// Tools whose successful results can be cached within a single run or turn (read-only).
pub const CACHEABLE_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "stat_file",
    "find_files",
    "search_code",
];

/// Deterministic cache key from tool name and JSON arguments (canonical serialization).
#[must_use]
pub fn tool_result_cache_key(tool_name: &str, input: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(input).unwrap_or_else(|_| format!("{input:?}"));
    format!("{tool_name}\0{canonical}")
}

// ---------------------------------------------------------------------------
// Agent loop parameters
// ---------------------------------------------------------------------------

/// Maximum tool-use iterations before the loop terminates.
///
/// Defaults to `usize::MAX` (effectively unlimited). Termination is
/// driven by `EndTurn` from the model, the credit/token budget,
/// cooperative cancellation, or an explicit caller-supplied
/// `SessionInit.max_turns` override (see
/// `aura_runtime::session::state::Session::apply_init`). Raised from
/// 25 because long-running batch workflows (e.g. task extraction
/// emitting many `create_task` calls) were silently terminated
/// mid-run with `stop_reason: "cancelled"` after hitting the cap, and
/// the wire format gives the UI no way to distinguish that case from
/// a user-initiated cancel.
pub const MAX_ITERATIONS: usize = usize::MAX;

/// Default exploration allowance (read-only tool calls before the
/// hard block in `detect_blocked_exploration` fires).
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05): the behavioral cap was removed because every gate
/// that key'd off it was hiding read-only loops rather than
/// breaking them. The detector remains in the codebase but is
/// now effectively unreachable; the only real terminators are
/// `EndTurn`, credit budget, and cooperative cancellation.
pub const DEFAULT_EXPLORATION_ALLOWANCE: usize = usize::MAX;

/// Auto-build cooldown: minimum iterations between automatic build checks.
pub const AUTO_BUILD_COOLDOWN: usize = 2;

/// Thinking budget taper: after this many iterations, reduce thinking budget.
pub const THINKING_TAPER_AFTER: usize = 2;

/// Factor by which to reduce the thinking budget each iteration after taper threshold.
pub const THINKING_TAPER_FACTOR: f64 = 0.6;

/// Minimum thinking budget after tapering.
///
/// Floor raised from 1024 to 6144 after harness runs showed the model
/// hitting `max_tokens` mid-`edit_file` — the partial tool_use JSON
/// recovered by `aura_reasoner::types::streaming` clocked in at
/// ~2.5 KB (~1000 tokens), plus preceding thinking/text, which the
/// previous 1024 floor could not fit. 6144 leaves room for one
/// full-size tool-call JSON plus a small amount of reasoning.
pub const THINKING_MIN_BUDGET: u32 = 6144;

/// Reasoner auto-enables Anthropic extended thinking on Claude 4.x
/// models when `max_tokens > 2048` (see
/// `aura_reasoner::anthropic::convert::resolve_thinking`). The agent
/// loop clamps `max_tokens` to this value when it needs to disable
/// extended thinking for one turn (iteration 0, force-tool steering)
/// — keeping the inequality strict ensures the auto-enable path
/// returns `None` for that one turn.
pub const THINKING_AUTO_ENABLE_THRESHOLD: u32 = 2048;

/// Maximum full reads of the same file before blocking.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05): re-reading the same file is a cache hit for
/// negligible tokens, not a behavioral failure mode worth
/// gating on.
pub const MAX_READS_PER_FILE: usize = usize::MAX;

/// Maximum range reads of the same file before blocking.
///
/// Neutralized to `usize::MAX`. Same rationale as
/// [`MAX_READS_PER_FILE`].
pub const MAX_RANGE_READS_PER_FILE: usize = usize::MAX;

/// Consecutive command failures before blocking all commands.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05). Commands either fail because the model needs to
/// try a different approach (which it can decide on its own) or
/// because the build is genuinely broken (the optional
/// `validate_build_preflight` gate handles that).
pub const CMD_FAILURE_BLOCK_THRESHOLD: usize = usize::MAX;

/// Consecutive write failures on a single file before blocking writes to it.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05).
pub const WRITE_FAILURE_BLOCK_THRESHOLD: usize = usize::MAX;

/// Stall detection: identical write targets for this many iterations triggers fail-fast.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05). The streak detector remains compiled but cannot
/// fire; the natural terminators (`EndTurn`, credit budget,
/// cancellation) cover the cases this used to catch.
pub const STALL_STREAK_THRESHOLD: usize = usize::MAX;

/// Budget warning at 30% utilization.
pub const BUDGET_WARNING_30: f64 = 0.30;

/// Budget warning at 40% (no writes yet) utilization.
pub const BUDGET_WARNING_40_NO_WRITE: f64 = 0.40;

/// Budget warning at 60% utilization (wrap up).
pub const BUDGET_WARNING_60: f64 = 0.60;

/// Characters per token estimate for context budget calculations.
pub const CHARS_PER_TOKEN: usize = 4;

/// Compaction tier thresholds (percentage of context used).
pub const COMPACTION_TIER_HISTORY: f64 = 0.85;

/// Aggressive compaction tier threshold.
pub const COMPACTION_TIER_AGGRESSIVE: f64 = 0.70;

/// 60% compaction tier threshold.
pub const COMPACTION_TIER_60: f64 = 0.60;

/// 30% compaction tier threshold.
pub const COMPACTION_TIER_30: f64 = 0.30;

/// Micro compaction tier threshold.
pub const COMPACTION_TIER_MICRO: f64 = 0.15;

/// Write file cooldown in iterations after a write failure.
///
/// Neutralized to `0` by the cook-loop-fix strip (2026-05): the
/// cooldown no longer schedules any wait, so the cooldown-block
/// detector is a no-op.
pub const WRITE_COOLDOWN_ITERATIONS: usize = 0;

/// Tools classified as exploration (read-only, non-modifying).
pub const EXPLORATION_TOOLS: &[&str] = &[
    "read_file",
    "list_files",
    "find_files",
    "stat_file",
    "search_code",
];

/// Tools that perform writes (mutations).
///
/// `apply_patch` is the unified dev-loop write primitive (Phase E of
/// harness-v2.2) and coexists with the granular `write_file` /
/// `edit_file` / `delete_file` tools that chat-mode and other agents
/// still use. All four count as forward progress for the read-only
/// steering counters and Phase B's `had_any_file_write` latch.
pub const WRITE_TOOLS: &[&str] =
    &["apply_patch", "write_file", "edit_file", "delete_file"];

/// Tools that run commands.
pub const COMMAND_TOOLS: &[&str] = &["run_command"];

/// Consecutive iterations where every tool call errors before forcing a stop.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05).
pub const CONSECUTIVE_ERROR_ITERATIONS_LIMIT: usize = usize::MAX;

/// Consecutive iterations containing at least one pathless
/// `write_file` / `edit_file` / `delete_file` block before forcing a
/// stop.
///
/// Neutralized to `usize::MAX` by the cook-loop-fix strip
/// (2026-05). The detector remains compiled but cannot fire.
pub const EMPTY_PATH_BLOCK_LIMIT: usize = usize::MAX;

// ---------------------------------------------------------------------------
// Write-side chunk guard
// ---------------------------------------------------------------------------

/// Per-turn soft cap on `write_file` content size. Calls exceeding this are
/// short-circuited with a synthetic error that asks the agent to write a
/// skeleton first and use `edit_file` appends for the rest. The goal is to
/// avoid re-echoing huge content into the next turn when the model
/// inevitably trips `max_tokens` mid-write.
/// Raised from 12_000 to 32_000 to give realistic explore/edit cycles headroom.
pub const WRITE_FILE_CHUNK_BYTES: usize = 32_000;

/// Hard ceiling on `write_file` content size. Reserved for future use by
/// executor-side enforcement; currently kept equal to
/// [`WRITE_FILE_CHUNK_BYTES`] so callers have one effective limit.
/// Raised from 12_000 to 32_000 to give realistic explore/edit cycles headroom.
pub const WRITE_FILE_HARD_MAX_BYTES: usize = 32_000;

// ---------------------------------------------------------------------------
// Narration budget (Phase 4 live steering)
// ---------------------------------------------------------------------------

/// Soft threshold (in output tokens) for consecutive tool-free narration
/// across turns. When the running total crosses this value, the loop
/// injects a synthetic user message demanding that the next turn be a
/// single tool call, then resets the counter. This is the live analog of
/// the Phase 2a `ForceToolCallNextTurn` hint.
pub const NARRATION_TOKEN_SOFT_BUDGET: usize = 1_500;

/// Hard ceiling (in output tokens) for consecutive tool-free narration.
/// Crossing this value is treated as a non-recoverable stall: the loop
/// stamps `AgentLoopResult::stop_reason_override` with
/// `"narration_budget_exhausted"` so downstream (the aura-automaton task
/// validator) can map it to a Phase 2b `NeedsDecomposition` outcome.
pub const NARRATION_TOKEN_HARD_BUDGET: usize = 4_000;

// ---------------------------------------------------------------------------
// Read-only loop steering (Phase 2 of harness-v2)
// ---------------------------------------------------------------------------

/// Soft threshold: after this many consecutive read-only iterations,
/// inject a synthetic user message demanding the next turn be a write
/// or `task_done`. Builds on the existing exploration budget — fires
/// even when the per-call exploration block hasn't tripped yet, because
/// the per-file caps in [`MAX_READS_PER_FILE`] / [`MAX_RANGE_READS_PER_FILE`]
/// can let an agent rack up ~40 reads across many files before the
/// hard block fires, by which point the credit budget is gone.
///
/// Orthogonal to [`NARRATION_TOKEN_SOFT_BUDGET`]: that one fires when
/// the model produces text but no tool calls; this one fires when the
/// model produces tool calls but they are all read-only.
///
/// Phase C of harness-v2.2 tightened this from `4` to `2`: crash
/// investigation showed the model burning 4 read-only iterations on a
/// small task (per-turn `tool_count=3`, `result_lens` 1187/2846/1225/409)
/// before the nudge fired, by which point most of the credit budget
/// was gone. Two read-only iterations is enough signal that the model
/// is grinding without writing; the nudge is a user message (not a
/// hard stop) so a legitimate "look twice then patch" pattern still
/// pays only one extra reminder message before the model gets to
/// write.
pub const READ_ONLY_INJECTION_THRESHOLD: usize = 2;

/// Hard threshold: after this many consecutive read-only iterations,
/// force `tool_choice = Required` and disable extended thinking for
/// the next turn so the model has no choice but to call a tool
/// (preferably a write). Anthropic blocks forced tool use while
/// extended thinking is enabled, so the two flips ride together.
///
/// Phase C of harness-v2.2 tightened this from `6` to `3`: paired with
/// the new [`READ_ONLY_INJECTION_THRESHOLD`] of 2, the steering is now
/// "one polite nudge after two read-only iterations, then force a tool
/// call on the third". Also dovetails with the Phase B EndTurn
/// intercept's attempt-3 escalation: that path saturates the read-only
/// counter at this threshold, so a single attempt-3 bump now escalates
/// straight to forced `tool_choice` on the very next iteration instead
/// of skipping a full intervention level (which is what the old
/// `6` value did — the bump landed one short of where it needed to).
pub const READ_ONLY_FORCE_TOOL_THRESHOLD: usize = 3;

// ---------------------------------------------------------------------------
// Dev-loop EndTurn completion contract (Phase B of harness-v2.2)
// ---------------------------------------------------------------------------

/// Cap on how many times `dispatch_stop_reason` may intercept a
/// `StopReason::EndTurn` for a dev-loop task that has not yet
/// produced a file write or called `task_done`. Each intercept escalates
/// severity: 1 = polite reminder, 2 = clamp thinking, 3 = force tool_choice.
/// After the cap, the loop exits and post-hoc validation
/// (`validate_execution`) catches the empty-write outcome.
pub const END_TURN_INTERCEPT_CAP: usize = 3;
