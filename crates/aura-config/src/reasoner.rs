//! Shared reasoner knobs (LLM retry envelope + thinking kill switches).
//!
//! Both `aura-reasoner` (in its `AnthropicConfig::from_env`) and the
//! agent streaming paths (`aura-agent`'s `stream_retry_params`) used
//! to parse these env vars independently. After Phase 1 both sides
//! consult [`ReasonerConfig`] so the values stay aligned.

use std::time::Duration;

use crate::env::{
    lookup_bool, lookup_numeric, AURA_DEV_LOOP_ENABLED_THINKING, AURA_LLM_BACKOFF_CAP_MS,
    AURA_LLM_BACKOFF_INITIAL_MS, AURA_LLM_MAX_RETRIES, FALSY_LITERALS, TRUTHY_LITERALS,
};

// ---------------------------------------------------------------------------
// Compile-time defaults
// ---------------------------------------------------------------------------

const DEFAULT_LLM_MAX_RETRIES: u32 = 8;
const DEFAULT_LLM_BACKOFF_INITIAL_MS: u64 = 250;
const DEFAULT_LLM_BACKOFF_CAP_MS: u64 = 30_000;

// ---------------------------------------------------------------------------
// LLM retry envelope
// ---------------------------------------------------------------------------

/// LLM retry envelope shared by `aura-reasoner` and the agent
/// streaming retry paths.
#[derive(Debug, Clone, Copy)]
pub struct LlmRetryConfig {
    /// env: `AURA_LLM_MAX_RETRIES` (default: `8`)
    ///
    /// Retry budget per outbound `/v1/messages` call.
    pub max_retries: u32,
    /// env: `AURA_LLM_BACKOFF_INITIAL_MS` (default: `250`)
    ///
    /// Initial backoff before the first retry. Doubled on each
    /// successive attempt up to [`Self::backoff_cap`].
    pub backoff_initial: Duration,
    /// env: `AURA_LLM_BACKOFF_CAP_MS` (default: `30_000`)
    ///
    /// Upper bound on the per-retry backoff.
    pub backoff_cap: Duration,
}

impl LlmRetryConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            max_retries: DEFAULT_LLM_MAX_RETRIES,
            backoff_initial: Duration::from_millis(DEFAULT_LLM_BACKOFF_INITIAL_MS),
            backoff_cap: Duration::from_millis(DEFAULT_LLM_BACKOFF_CAP_MS),
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] when one of the numeric
    /// overrides is non-empty but unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        if let Some(retries) = lookup_numeric::<u32>(AURA_LLM_MAX_RETRIES)? {
            cfg.max_retries = retries;
        }
        if let Some(initial_ms) = lookup_numeric::<u64>(AURA_LLM_BACKOFF_INITIAL_MS)? {
            cfg.backoff_initial = Duration::from_millis(initial_ms);
        }
        if let Some(cap_ms) = lookup_numeric::<u64>(AURA_LLM_BACKOFF_CAP_MS)? {
            cfg.backoff_cap = Duration::from_millis(cap_ms);
        }
        Ok(cfg)
    }

    /// Helper for the agent streaming retry loop which still wants the
    /// raw `(max_retries, initial_ms, cap_ms)` triple. Lets us migrate
    /// the call site to one accessor without changing the surrounding
    /// math.
    #[must_use]
    pub fn as_legacy_triple(&self) -> (u32, u64, u64) {
        let initial = u64::try_from(self.backoff_initial.as_millis())
            .unwrap_or(DEFAULT_LLM_BACKOFF_INITIAL_MS);
        let cap = u64::try_from(self.backoff_cap.as_millis()).unwrap_or(DEFAULT_LLM_BACKOFF_CAP_MS);
        (self.max_retries, initial, cap)
    }
}

impl Default for LlmRetryConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// Reasoner-side thinking knobs
// ---------------------------------------------------------------------------

/// Reasoner-side thinking kill switches.
///
/// Currently houses the historical
/// `AURA_DEV_LOOP_ENABLED_THINKING` kill switch. The reasoner removed
/// the `enabled`-mode escalation it gated when Anthropic dropped
/// `thinking.type=enabled` for the Claude 4 family (May 2026), so the
/// switch is preserved as a typed knob for parity with prior tooling
/// and as the migration surface for any future reasoner-side
/// thinking-mode toggles.
#[derive(Debug, Clone, Copy)]
pub struct ReasonerThinkingConfig {
    /// env: `AURA_DEV_LOOP_ENABLED_THINKING` (default: `false`)
    ///
    /// Historical dev-loop kill switch for the Claude 3.7 `enabled`
    /// thinking-mode escalation. Currently observed but not acted on
    /// after the May 2026 Anthropic API change; kept here as the
    /// single owned reader.
    pub dev_loop_enabled_thinking: bool,
}

impl ReasonerThinkingConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            dev_loop_enabled_thinking: false,
        }
    }

    /// Apply env overrides. Infallible.
    ///
    /// # Errors
    ///
    /// Currently always returns `Ok`; the signature is preserved for
    /// API symmetry with the other sub-configs.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        cfg.dev_loop_enabled_thinking = lookup_bool(
            AURA_DEV_LOOP_ENABLED_THINKING,
            false,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        Ok(cfg)
    }
}

impl Default for ReasonerThinkingConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// Reasoner config wrapper
// ---------------------------------------------------------------------------

/// Reasoner-layer config: LLM retry envelope + thinking knobs.
#[derive(Debug, Clone, Copy)]
pub struct ReasonerConfig {
    /// See [`LlmRetryConfig`].
    pub llm_retry: LlmRetryConfig,
    /// See [`ReasonerThinkingConfig`].
    pub thinking: ReasonerThinkingConfig,
}

impl ReasonerConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            llm_retry: LlmRetryConfig::defaults(),
            thinking: ReasonerThinkingConfig::defaults(),
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] when one of the numeric LLM
    /// retry overrides is non-empty but unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        Ok(Self {
            llm_retry: LlmRetryConfig::from_env()?,
            thinking: ReasonerThinkingConfig::from_env()?,
        })
    }
}

impl Default for ReasonerConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
