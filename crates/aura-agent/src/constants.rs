//! Product constants and numeric parameters for the agent layer.
//!
//! # Model selection
//!
//! Historically this module exported `DEFAULT_MODEL` (and `FALLBACK_MODEL`)
//! that the agent loop, agent runner, and session bootstrap silently fell
//! back to whenever a caller forgot to thread the user-selected model
//! through. That was the regression that shipped `claude-opus-4-6`
//! production traffic when the WS session had asked for `claude-opus-4-7`.
//!
//! Both constants have been removed. The single surviving fallback,
//! [`aura_reasoner::ENV_FALLBACK_MODEL`], lives behind the provider
//! boundary and is used **only** when `AURA_DEFAULT_MODEL` is unset
//! during provider construction. Every layer above that must receive an
//! explicit model — see [`crate::AgentLoopConfig::for_agent`] and
//! [`crate::agent_runner::AgentRunnerConfig::for_agent`].

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
/// Derived from [`aura_core::MAX_TURNS`] — the single source of truth
/// for every "max turns / max iterations" knob in the system. See the
/// constant's doc comment for the full list of consumers. Termination
/// is still driven by `EndTurn` from the model, the credit/token
/// budget, cooperative cancellation, or an explicit caller-supplied
/// `SessionInit.max_turns` override (see
/// `aura_runtime::session::state::Session::apply_init`); this cap is
/// the hard ceiling that guards against runaway loops.
pub const MAX_ITERATIONS: usize = aura_core::MAX_TURNS as usize;

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
/// All three count as forward progress for the read-only steering
/// counters and Phase B's `had_any_file_write` latch.
pub const WRITE_TOOLS: &[&str] = &["write_file", "edit_file", "delete_file"];

/// Tools that run commands.
pub const COMMAND_TOOLS: &[&str] = &["run_command"];

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
