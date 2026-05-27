//! # aura-config
//!
//! Single source of truth for **agent behavior knobs** and the **shared
//! LLM retry / thinking env vars** consumed by `aura-agent`,
//! `aura-automaton`, and `aura-reasoner`. After Phase 1 of the
//! core-loop architecture refactor, *every* numeric magic value, tool
//! list, default budget, and owned env var that controls agent
//! behavior lives here. "Where is this knob read?" and "what is this
//! number?" have exactly one answer.
//!
//! `cargo doc -p aura-config` renders the canonical knobs catalog.
//!
//! ## Surfaces
//!
//! - [`AuraConfig`] — the root config struct. Two sub-trees:
//!   [`AgentConfig`] (agent / prompts / automaton knobs) and
//!   [`ReasonerConfig`] (shared LLM retry + thinking knobs that both
//!   `aura-reasoner` and the agent streaming paths consult).
//! - [`AuraConfig::defaults`] — `const fn`, no env access. The
//!   compile-time fallback every caller eventually rolls back to.
//! - [`AuraConfig::from_env`] — parses every owned env override once
//!   at startup. Errors carry the env-var name.
//! - [`loaded`] — process-wide singleton (initialized lazily from
//!   [`AuraConfig::from_env`] on first access).
//! - [`install_for_test`] — RAII guard that swaps the singleton for
//!   the lifetime of the returned [`ConfigGuard`] so unit tests can
//!   override behavior uniformly without racing the process env.
//! - Sub-crate accessor helpers: [`agent`] (shortcut for
//!   `loaded().agent`) and [`reasoner`] (shortcut for
//!   `loaded().reasoner`).
//!
//! ## Owned env vars
//!
//! These names are owned by this crate. No other crate in the workspace
//! may call `std::env::var(...)` on them — the boundary is enforced by
//! `tests/config_boundary.rs` at workspace root.
//!
//! | Env var | Type | Default | Field |
//! | --- | --- | --- | --- |
//! | `AURA_AGENT_DISABLE_COMPACTION` | bool | `false` | [`CompactionConfig::disabled`] |
//! | `AURA_AGENT_IMPLEMENT_NOW` | bool | `true` | [`SteeringConfig::implement_now_enabled`] |
//! | `AURA_AGENT_IMPLEMENT_NOW_THRESHOLD` | usize | `10` | [`SteeringConfig::implement_now_threshold`] |
//! | `AURA_AGENT_IMPLEMENT_NOW_BLOCK` | bool | `true` | [`SteeringConfig::implement_now_hard_block`] |
//! | `AURA_AGENT_BOOTSTRAP_SPEC_BYTES` | usize | `1500` | [`PromptsConfig::bootstrap_spec_bytes`] |
//! | `AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES` | bool | `false` | [`PromptsConfig::bootstrap_strip_code_fences`] |
//! | `AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS` | usize | `12_000` | [`PromptsConfig::bootstrap_context_chars`] |
//! | `AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS` | u64 (clamped) | `10` | [`ToolsConfig::heartbeat_interval`] |
//! | `AURA_DOD_TEST_COMMAND` | string | unset | [`VerifyConfig::test_command_override`] |
//! | `AURA_SIMPLE_MODEL` | string | unset | [`AgentConfig::simple_model_override`] |
//! | `AURA_LLM_MAX_RETRIES` | u32 | `8` | [`LlmRetryConfig::max_retries`] |
//! | `AURA_LLM_BACKOFF_INITIAL_MS` | u64 | `250` | [`LlmRetryConfig::backoff_initial`] (millis) |
//! | `AURA_LLM_BACKOFF_CAP_MS` | u64 | `30_000` | [`LlmRetryConfig::backoff_cap`] (millis) |
//! | `AURA_DEV_LOOP_ENABLED_THINKING` | bool | `false` | [`ReasonerThinkingConfig::dev_loop_enabled_thinking`] |
//!
//! Tool-sandbox knobs (`file_ops::SKIP_DIRS`, `INCLUDE_EXTENSIONS`),
//! console formatting constants, transport-layer debug / emergency /
//! WAF env vars (`AURA_LLM_WAF_SAFE_JSON`,
//! `AURA_LLM_EMERGENCY_BODY_CAP_BYTES`, `AURA_LLM_DEBUG_REQUEST_DUMP_DIR`,
//! `AURA_DEBUG_CLOUDFLARE_DUMP_DIR`), and auth/session vars
//! (`AURA_ROUTER_JWT`) deliberately stay in their owning boundary.
//!
//! ## Pure compile-time constants
//!
//! Names exported as plain `pub const` from the crate root because they
//! are pure compile-time values (no env override, no per-process
//! tuning): [`CACHEABLE_TOOLS`], [`EXPLORATION_TOOLS`], [`WRITE_TOOLS`],
//! [`COMMAND_TOOLS`], [`MAX_ITERATIONS`], [`AUTO_BUILD_COOLDOWN`],
//! [`THINKING_TAPER_AFTER`], [`THINKING_TAPER_FACTOR`],
//! [`THINKING_MIN_BUDGET`], [`THINKING_AUTO_ENABLE_THRESHOLD`],
//! [`BUDGET_WARNING_30`], [`BUDGET_WARNING_40_NO_WRITE`],
//! [`BUDGET_WARNING_60`], [`CHARS_PER_TOKEN`],
//! [`COMPACTION_TIER_HISTORY`], [`COMPACTION_TIER_AGGRESSIVE`],
//! [`COMPACTION_TIER_60`], [`COMPACTION_TIER_30`],
//! [`COMPACTION_TIER_MICRO`], [`WRITE_FILE_CHUNK_BYTES`],
//! [`WRITE_FILE_HARD_MAX_BYTES`], [`MAX_TASK_CONTEXT_CHARS`],
//! [`MAX_WORK_LOG_TASK_CONTEXT`], [`MAX_STUB_FIX_ATTEMPTS`],
//! [`RECENT_OUTCOMES_WINDOW`], [`PERVASIVE_ERROR_MIN_CALLS`],
//! [`PERVASIVE_ERROR_THRESHOLD`],
//! [`MIN_TOOL_HEARTBEAT_INTERVAL_SECS`],
//! [`MAX_TOOL_HEARTBEAT_INTERVAL_SECS`],
//! [`DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS`],
//! [`READS_AFTER_WRITE_ALLOWANCE`], [`TOOL_ERROR_PREVIEW_LIMIT`],
//! [`MAX_OUTPUT_BYTES`], [`BUILD_TIMEOUT_SECS`],
//! [`BUILD_FIX_SNAPSHOT_BUDGET`], [`RESOLVE_BUDGET`],
//! [`MAX_TYPE_FILES`], [`ERROR_SOURCE_BUDGET`], [`GIT_READ_TIMEOUT_SECS`],
//! [`REPEATED_READ_THRESHOLD`],
//! [`REPEATED_READ_HASH_DISPLAY_CHARS`],
//! [`IMPLEMENT_NOW_DEFAULT_THRESHOLD`],
//! [`IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE`],
//! [`PROMPT_COMPACTION_MAX_BLOCK_CHARS`],
//! [`REFINEMENT_MAX_TOKENS`], [`SPEC_GEN_MAX_TOKENS`],
//! [`DEV_LOOP_RETRY_NOTE_MAX_BYTES`], [`tool_result_cache_key`].
//!
//! All of these are also reachable via the corresponding sub-config
//! struct (e.g. [`ToolsConfig::cacheable_tools`]); the crate-level
//! constants are kept for ergonomic `aura_config::CACHEABLE_TOOLS`
//! access from existing call sites.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod agent;
pub mod automaton;
pub mod compaction;
pub mod env;
pub mod prompts;
pub mod reasoner;
pub mod steering;
pub mod thinking;
pub mod tools;
pub mod verify;

use std::sync::{Mutex, OnceLock, PoisonError};

pub use agent::AgentConfig;
pub use automaton::AutomatonConfig;
pub use compaction::CompactionConfig;
pub use env::{ConfigError, ENV_VAR_NAMES};
pub use prompts::PromptsConfig;
pub use reasoner::{LlmRetryConfig, ReasonerConfig, ReasonerThinkingConfig};
pub use steering::SteeringConfig;
pub use thinking::ThinkingConfig;
pub use tools::ToolsConfig;
pub use verify::VerifyConfig;

// ---------------------------------------------------------------------------
// Crate-level pure constants (re-exported for ergonomic call-site access).
// ---------------------------------------------------------------------------

pub use agent::{
    DEFAULT_BOOTSTRAP_TASK_CONTEXT_CHARS, MAX_ITERATIONS, MAX_TASK_CONTEXT_CHARS,
    MAX_WORK_LOG_TASK_CONTEXT, PERVASIVE_ERROR_MIN_CALLS, PERVASIVE_ERROR_THRESHOLD,
    RECENT_OUTCOMES_WINDOW,
};
pub use automaton::{DEV_LOOP_RETRY_NOTE_MAX_BYTES, REFINEMENT_MAX_TOKENS, SPEC_GEN_MAX_TOKENS};
pub use compaction::{
    COMPACTION_TIER_30, COMPACTION_TIER_60, COMPACTION_TIER_AGGRESSIVE, COMPACTION_TIER_HISTORY,
    COMPACTION_TIER_MICRO,
};
pub use prompts::{
    BOOTSTRAP_SPEC_DEFAULT_BYTES, PROMPT_COMPACTION_MAX_BLOCK_CHARS,
    REPEATED_READ_HASH_DISPLAY_CHARS,
};
pub use steering::{
    IMPLEMENT_NOW_DEFAULT_THRESHOLD, IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE, REPEATED_READ_THRESHOLD,
};
pub use thinking::{
    AUTO_BUILD_COOLDOWN, BUDGET_WARNING_30, BUDGET_WARNING_40_NO_WRITE, BUDGET_WARNING_60,
    CHARS_PER_TOKEN, MAX_STUB_FIX_ATTEMPTS, THINKING_AUTO_ENABLE_THRESHOLD, THINKING_MIN_BUDGET,
    THINKING_TAPER_AFTER, THINKING_TAPER_FACTOR,
};
pub use tools::{
    tool_result_cache_key, CACHEABLE_TOOLS, COMMAND_TOOLS, DEFAULT_TOOL_HEARTBEAT_INTERVAL_SECS,
    EXPLORATION_TOOLS, MAX_TOOL_HEARTBEAT_INTERVAL_SECS, MIN_TOOL_HEARTBEAT_INTERVAL_SECS,
    READS_AFTER_WRITE_ALLOWANCE, TOOL_ERROR_PREVIEW_LIMIT, WRITE_FILE_CHUNK_BYTES,
    WRITE_FILE_HARD_MAX_BYTES, WRITE_TOOLS,
};
pub use verify::{
    BUILD_FIX_SNAPSHOT_BUDGET, BUILD_TIMEOUT_SECS, ERROR_SOURCE_BUDGET, GIT_READ_TIMEOUT_SECS,
    MAX_OUTPUT_BYTES, MAX_TYPE_FILES, RESOLVE_BUDGET,
};

// ---------------------------------------------------------------------------
// Root config struct
// ---------------------------------------------------------------------------

/// Root configuration tree.
///
/// Holds the [`AgentConfig`] sub-tree (consumed by `aura-agent`,
/// `aura-automaton`, and the future `aura-prompts` crate) and the
/// [`ReasonerConfig`] sub-tree (consumed by `aura-reasoner` and the
/// agent streaming retry paths).
#[derive(Debug, Clone)]
pub struct AuraConfig {
    /// Agent / prompts / automaton knobs.
    pub agent: AgentConfig,
    /// Shared LLM retry and thinking knobs.
    pub reasoner: ReasonerConfig,
}

impl AuraConfig {
    /// Compile-time defaults. No env access.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            agent: AgentConfig::defaults(),
            reasoner: ReasonerConfig::defaults(),
        }
    }

    /// Parse every owned env override exactly once.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] with the env-var name in context when a
    /// numeric override is non-empty but unparseable. Unset env vars
    /// silently fall through to defaults.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            agent: AgentConfig::from_env()?,
            reasoner: ReasonerConfig::from_env()?,
        })
    }
}

impl Default for AuraConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

// ---------------------------------------------------------------------------
// Process-wide singleton + test override guard
// ---------------------------------------------------------------------------

/// Container holding the currently-installed [`AuraConfig`]. Wrapped in
/// a `Mutex` so [`install_for_test`] can swap the value atomically
/// without leaking a reference past the guard's lifetime.
fn slot() -> &'static Mutex<AuraConfig> {
    static SLOT: OnceLock<Mutex<AuraConfig>> = OnceLock::new();
    SLOT.get_or_init(|| {
        let cfg = AuraConfig::from_env().unwrap_or_else(|err| {
            tracing::warn!(
                error = %err,
                "aura-config: failed to parse env overrides; falling back to defaults"
            );
            AuraConfig::defaults()
        });
        Mutex::new(cfg)
    })
}

/// The currently-installed process-wide config as an owned snapshot.
///
/// Reads the slot under the process-wide mutex and clones. Returning
/// owned (rather than `&'static`) is deliberate: it lets
/// [`install_for_test`] swap the slot mid-run and have downstream
/// callers observe the override without a stale-cache hazard.
#[must_use]
pub fn loaded() -> AuraConfig {
    current()
}

/// Shortcut for `loaded().agent`. Use this from agent / automaton /
/// prompts call sites instead of `loaded()`.
///
/// Returns an owned [`AgentConfig`]. The clone is cheap (mostly
/// `Copy` fields plus a small handful of `String`/`Vec`); see the
/// crate-level docs for the per-field cost discussion.
#[must_use]
pub fn agent() -> AgentConfig {
    current().agent
}

/// Shortcut for `loaded().reasoner`. Use this from reasoner call
/// sites (shared LLM retry + thinking knobs).
#[must_use]
pub fn reasoner() -> ReasonerConfig {
    current().reasoner
}

/// RAII guard returned by [`install_for_test`]. Restores the previous
/// config when dropped.
///
/// # Cross-test safety
///
/// Tests that install a config must keep the guard alive for the
/// duration of the override. The implementation uses a process-wide
/// mutex; tests that mutate config in parallel should coordinate
/// through that mutex (e.g. via a shared `Mutex<()>`) to keep
/// observed behavior deterministic.
#[must_use = "ConfigGuard restores the previous config on drop; binding to `_` would end the override immediately"]
pub struct ConfigGuard {
    previous: Option<AuraConfig>,
}

impl Drop for ConfigGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.previous.take() {
            let mut guard = slot().lock().unwrap_or_else(PoisonError::into_inner);
            *guard = prev;
        }
    }
}

/// Swap the process-wide config for the duration of the returned
/// [`ConfigGuard`]. Intended for `#[cfg(test)]` use: production code
/// should never call this.
///
/// Note that [`loaded`] caches a `&'static AuraConfig` snapshot the
/// first time it is called per process; tests that need to read the
/// installed config should use the fields directly via the
/// [`ConfigGuard`] interface (e.g. observing behavior through
/// downstream functions that re-read the slot) rather than relying on
/// `loaded()` reflecting the override.
pub fn install_for_test(cfg: AuraConfig) -> ConfigGuard {
    let mut guard = slot().lock().unwrap_or_else(PoisonError::into_inner);
    let previous = std::mem::replace(&mut *guard, cfg);
    ConfigGuard {
        previous: Some(previous),
    }
}

/// Read the *current* installed config snapshot, ignoring the
/// `loaded()` cache. Used by [`install_for_test`]-aware test code
/// (and by the agent's per-call accessors that need to honor the
/// override).
#[must_use]
pub fn current() -> AuraConfig {
    slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Tests that install_for_test serialize through this mutex so
    /// the process-wide slot they share doesn't race when cargo runs
    /// the tests in parallel.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_are_const_evaluable() {
        const _DEFAULTS: AuraConfig = AuraConfig::defaults();
    }

    #[test]
    fn from_env_returns_a_config() {
        let _ = AuraConfig::from_env().expect("from_env defaults must succeed");
    }

    #[test]
    fn install_for_test_restores_previous_on_drop() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let original = current();
        {
            let mut overridden = AuraConfig::defaults();
            overridden.agent.compaction.disabled = !original.agent.compaction.disabled;
            let _guard = install_for_test(overridden.clone());
            assert_eq!(
                current().agent.compaction.disabled,
                overridden.agent.compaction.disabled,
            );
        }
        assert_eq!(
            current().agent.compaction.disabled,
            original.agent.compaction.disabled,
        );
    }

    #[test]
    fn loaded_returns_an_owned_snapshot() {
        let _: AuraConfig = loaded();
        let _: AgentConfig = agent();
        let _: ReasonerConfig = reasoner();
    }

    #[test]
    fn install_for_test_is_observable_through_accessors() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let baseline = current();
        let mut overridden = baseline.clone();
        overridden.agent.compaction.disabled = !baseline.agent.compaction.disabled;
        let _guard = install_for_test(overridden.clone());
        assert_eq!(
            agent().compaction.disabled,
            overridden.agent.compaction.disabled,
            "agent() must observe install_for_test swaps"
        );
    }
}
