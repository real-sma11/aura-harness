//! Compaction kill switch + tier thresholds.

use crate::env::{lookup_bool, AURA_AGENT_DISABLE_COMPACTION, FALSY_LITERALS, TRUTHY_LITERALS};

// ---------------------------------------------------------------------------
// Compile-time tier thresholds (percentage-of-context utilization).
// ---------------------------------------------------------------------------

/// Tier-A history compaction kicks in at 85% utilization.
pub const COMPACTION_TIER_HISTORY: f64 = 0.85;

/// Aggressive compaction tier (70% utilization).
pub const COMPACTION_TIER_AGGRESSIVE: f64 = 0.70;

/// 60% utilization tier.
pub const COMPACTION_TIER_60: f64 = 0.60;

/// 30% utilization tier.
pub const COMPACTION_TIER_30: f64 = 0.30;

/// Micro-compaction tier (15% utilization).
pub const COMPACTION_TIER_MICRO: f64 = 0.15;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Compaction config: the operator kill switch plus the five tier
/// thresholds.
#[derive(Debug, Clone, Copy)]
pub struct CompactionConfig {
    /// env: `AURA_AGENT_DISABLE_COMPACTION` (default: `false`)
    ///
    /// When `true`, every compaction entry point in the agent loop
    /// becomes a no-op. Used to test whether read-spiral behaviour is
    /// being driven by older `read_file` results getting truncated
    /// out of context.
    pub disabled: bool,
    /// See [`COMPACTION_TIER_HISTORY`].
    pub tier_history: f64,
    /// See [`COMPACTION_TIER_AGGRESSIVE`].
    pub tier_aggressive: f64,
    /// See [`COMPACTION_TIER_60`].
    pub tier_60: f64,
    /// See [`COMPACTION_TIER_30`].
    pub tier_30: f64,
    /// See [`COMPACTION_TIER_MICRO`].
    pub tier_micro: f64,
}

impl CompactionConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            disabled: false,
            tier_history: COMPACTION_TIER_HISTORY,
            tier_aggressive: COMPACTION_TIER_AGGRESSIVE,
            tier_60: COMPACTION_TIER_60,
            tier_30: COMPACTION_TIER_30,
            tier_micro: COMPACTION_TIER_MICRO,
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Currently infallible (no parse step), but returns `Result` for
    /// API parity with the other sub-config constructors so future
    /// numeric overrides can be added without churning call sites.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        cfg.disabled = lookup_bool(
            AURA_AGENT_DISABLE_COMPACTION,
            false,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        Ok(cfg)
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
