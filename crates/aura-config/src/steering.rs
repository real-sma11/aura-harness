//! `implement_now` / repeated-read steering knobs.

use crate::env::{
    lookup_bool, lookup_nonzero_usize, AURA_AGENT_IMPLEMENT_NOW, AURA_AGENT_IMPLEMENT_NOW_BLOCK,
    AURA_AGENT_IMPLEMENT_NOW_THRESHOLD, FALSY_LITERALS, TRUTHY_LITERALS,
};

// ---------------------------------------------------------------------------
// Compile-time constants
// ---------------------------------------------------------------------------

/// Default exploration-tool count before the harness fires the
/// `implement_now` soft nudge.
pub const IMPLEMENT_NOW_DEFAULT_THRESHOLD: usize = 10;

/// Maximum read-only paths surfaced in the `implement_now` message
/// body. Keeps the nudge readable when the agent has read dozens of
/// files.
pub const IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE: usize = 5;

/// Threshold at which a single `content_hash` triggers a repeated-read
/// nudge. Compile-time only.
pub const REPEATED_READ_THRESHOLD: usize = 3;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Steering thresholds and the `implement_now` enable / hard-block
/// switches.
#[derive(Debug, Clone, Copy)]
pub struct SteeringConfig {
    /// env: `AURA_AGENT_IMPLEMENT_NOW` (default: `true`)
    ///
    /// Master switch for the `implement_now` soft nudge. When
    /// `false`, the gate evaluator returns early without queuing the
    /// nudge.
    pub implement_now_enabled: bool,
    /// env: `AURA_AGENT_IMPLEMENT_NOW_THRESHOLD` (default: `10`)
    ///
    /// Exploration-tool count after which the nudge fires.
    pub implement_now_threshold: usize,
    /// env: `AURA_AGENT_IMPLEMENT_NOW_BLOCK` (default: `true`)
    ///
    /// When `true`, the pre-dispatch hard block also rejects any
    /// further read/search tool calls once the soft nudge has fired
    /// without any file writes landing.
    pub implement_now_hard_block: bool,
    /// Compile-time only (default: [`REPEATED_READ_THRESHOLD`] = `3`).
    ///
    /// Threshold the repeated-read tracker uses to enqueue a nudge.
    pub repeated_read_threshold: usize,
    /// Compile-time only (default:
    /// [`IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE`] = `5`). Cap on how many
    /// read paths the gate surfaces in the rendered message body.
    pub implement_now_max_paths_in_message: usize,
}

impl SteeringConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            implement_now_enabled: true,
            implement_now_threshold: IMPLEMENT_NOW_DEFAULT_THRESHOLD,
            implement_now_hard_block: true,
            repeated_read_threshold: REPEATED_READ_THRESHOLD,
            implement_now_max_paths_in_message: IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE,
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] if
    /// `AURA_AGENT_IMPLEMENT_NOW_THRESHOLD` is non-empty but
    /// unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        cfg.implement_now_enabled = lookup_bool(
            AURA_AGENT_IMPLEMENT_NOW,
            true,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        cfg.implement_now_hard_block = lookup_bool(
            AURA_AGENT_IMPLEMENT_NOW_BLOCK,
            true,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        if let Some(threshold) = lookup_nonzero_usize(AURA_AGENT_IMPLEMENT_NOW_THRESHOLD)? {
            cfg.implement_now_threshold = threshold;
        }
        Ok(cfg)
    }
}

impl Default for SteeringConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
