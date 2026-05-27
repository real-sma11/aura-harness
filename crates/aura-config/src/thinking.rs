//! Thinking-budget taper, auto-build cooldown, budget warnings, and
//! related agent-loop pacing constants.

// ---------------------------------------------------------------------------
// Compile-time constants
// ---------------------------------------------------------------------------

/// Auto-build cooldown: minimum iterations between automatic build
/// checks.
pub const AUTO_BUILD_COOLDOWN: usize = 2;

/// Thinking budget taper: after this many iterations, reduce thinking
/// budget by [`THINKING_TAPER_FACTOR`].
pub const THINKING_TAPER_AFTER: usize = 2;

/// Factor by which to reduce the thinking budget each iteration after
/// the taper threshold.
pub const THINKING_TAPER_FACTOR: f64 = 0.6;

/// Minimum thinking budget after tapering.
pub const THINKING_MIN_BUDGET: u32 = 6144;

/// Threshold at which the Anthropic reasoner auto-enables extended
/// thinking. The agent loop clamps `max_tokens` at this value when it
/// needs to suppress thinking for one turn (iteration 0 / force-tool).
pub const THINKING_AUTO_ENABLE_THRESHOLD: u32 = 2048;

/// Budget warning at 30% utilization.
pub const BUDGET_WARNING_30: f64 = 0.30;

/// Budget warning at 40% utilization (no writes yet).
pub const BUDGET_WARNING_40_NO_WRITE: f64 = 0.40;

/// Budget warning at 60% utilization (wrap-up nudge).
pub const BUDGET_WARNING_60: f64 = 0.60;

/// Characters-per-token estimate for context budget calculations.
pub const CHARS_PER_TOKEN: usize = 4;

/// Cap on automatic stub-fix retries before surfacing to the user.
pub const MAX_STUB_FIX_ATTEMPTS: u32 = 2;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Thinking + pacing knobs.
///
/// All values are compile-time today; no env overrides apply.
#[derive(Debug, Clone, Copy)]
pub struct ThinkingConfig {
    /// See [`AUTO_BUILD_COOLDOWN`].
    pub auto_build_cooldown: usize,
    /// See [`THINKING_TAPER_AFTER`].
    pub thinking_taper_after: usize,
    /// See [`THINKING_TAPER_FACTOR`].
    pub thinking_taper_factor: f64,
    /// See [`THINKING_MIN_BUDGET`].
    pub thinking_min_budget: u32,
    /// See [`THINKING_AUTO_ENABLE_THRESHOLD`].
    pub thinking_auto_enable_threshold: u32,
    /// See [`BUDGET_WARNING_30`].
    pub budget_warning_30: f64,
    /// See [`BUDGET_WARNING_40_NO_WRITE`].
    pub budget_warning_40_no_write: f64,
    /// See [`BUDGET_WARNING_60`].
    pub budget_warning_60: f64,
    /// See [`CHARS_PER_TOKEN`].
    pub chars_per_token: usize,
    /// See [`MAX_STUB_FIX_ATTEMPTS`].
    pub max_stub_fix_attempts: u32,
}

impl ThinkingConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            auto_build_cooldown: AUTO_BUILD_COOLDOWN,
            thinking_taper_after: THINKING_TAPER_AFTER,
            thinking_taper_factor: THINKING_TAPER_FACTOR,
            thinking_min_budget: THINKING_MIN_BUDGET,
            thinking_auto_enable_threshold: THINKING_AUTO_ENABLE_THRESHOLD,
            budget_warning_30: BUDGET_WARNING_30,
            budget_warning_40_no_write: BUDGET_WARNING_40_NO_WRITE,
            budget_warning_60: BUDGET_WARNING_60,
            chars_per_token: CHARS_PER_TOKEN,
            max_stub_fix_attempts: MAX_STUB_FIX_ATTEMPTS,
        }
    }
}

impl Default for ThinkingConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
