//! Fleet daemon + spawn knobs (Phase 4a).
//!
//! These values are read by the future fleet daemon / spawn plumbing
//! (Phases 5+); the struct lives in `aura-config` so the single
//! source of truth for "how big is the fleet" stays here even before
//! the daemon crate exists.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Every field has a sane default so a zero-config user sees the
//!   same behaviour as today (no fleet daemon, single in-process
//!   agent loop).
//! - `max_concurrent_agents` of `0` is intentionally accepted from
//!   env overrides but [`FleetConfig::defaults`] always returns a
//!   non-zero value. Consumers that read the knob must treat `0` as
//!   "fall back to default" rather than "no agents", per the plan
//!   §4 sketch.
//! - `from_env` is the only fallible surface; it inherits the same
//!   "blank vars fall through to defaults" semantics as the other
//!   sub-configs (see [`crate::env::lookup_numeric`]).
//!
//! ## Owned env vars
//!
//! | Var | Type | Default | Field |
//! | --- | --- | --- | --- |
//! | `AURA_FLEET_EMBEDDED_DAEMON` | bool | `true` | [`FleetConfig::embedded_daemon`] |
//! | `AURA_FLEET_MAX_CONCURRENT_AGENTS` | u32 | `32` | [`FleetConfig::max_concurrent_agents`] |
//! | `AURA_FLEET_SHUTDOWN_GRACE_MS` | u64 | `30_000` | [`FleetConfig::shutdown_grace_ms`] |
//! | `AURA_FLEET_ORPHAN_ON_PARENT_DEATH` | bool | `true` | [`FleetConfig::orphan_on_parent_death`] |
//! | `AURA_FLEET_DEFAULT_MODE` | string | `agent` | [`FleetConfig::default_mode`] |

use aura_core_modes::AgentMode;
use serde::{Deserialize, Serialize};

use crate::env::{
    lookup_bool, lookup_numeric, AURA_FLEET_DEFAULT_MODE, AURA_FLEET_EMBEDDED_DAEMON,
    AURA_FLEET_MAX_CONCURRENT_AGENTS, AURA_FLEET_ORPHAN_ON_PARENT_DEATH,
    AURA_FLEET_SHUTDOWN_GRACE_MS, FALSY_LITERALS, TRUTHY_LITERALS,
};

const DEFAULT_EMBEDDED_DAEMON: bool = true;
const DEFAULT_MAX_CONCURRENT_AGENTS: u32 = 32;
const DEFAULT_SHUTDOWN_GRACE_MS: u64 = 30_000;
const DEFAULT_ORPHAN_ON_PARENT_DEATH: bool = true;
/// Daemon-default rung of the Phase 9 [`AgentMode`] resolution
/// priority. Overridable via `AURA_FLEET_DEFAULT_MODE`; falls
/// through to [`AgentMode::Agent`] which is also the absolute
/// fallback at the bottom of the resolution chain.
const DEFAULT_FLEET_MODE: AgentMode = AgentMode::Agent;

/// Fleet daemon + spawn knobs. See the module-level docs for invariants.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, rename_all = "snake_case")]
pub struct FleetConfig {
    /// Embedded daemon (in-process) when true; external when false.
    /// Default: `true` (current single-binary mode).
    pub embedded_daemon: bool,
    /// Max concurrent agent loops across the fleet.
    ///
    /// `0` from env overrides is allowed but consumers must treat it
    /// as "fall back to default". [`FleetConfig::defaults`] always
    /// returns a non-zero value.
    pub max_concurrent_agents: u32,
    /// Graceful-shutdown grace period (ms) before children are killed.
    pub shutdown_grace_ms: u64,
    /// On parent process death, detached children: detach (`true`) or
    /// cancel (`false`).
    pub orphan_on_parent_death: bool,
    /// Daemon-default rung of the Phase 9 [`AgentMode`] resolution
    /// priority chain.
    ///
    /// The chain (highest precedence first):
    /// 1. CLI flag (`aura --mode <name>`)
    /// 2. TUI `/mode <name>` slash command
    /// 3. SDK `SessionConfig::mode`
    /// 4. **This field** — daemon-wide default
    /// 5. [`AgentMode::Agent`] absolute fallback
    ///
    /// Operators set this through `AURA_FLEET_DEFAULT_MODE` (env)
    /// or the `[fleet] default_mode = "..."` TOML key; both expect
    /// the lower-snake serde representation
    /// (`agent|plan|ask|debug`).
    pub default_mode: AgentMode,
}

impl FleetConfig {
    /// Compile-time defaults. No env access.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            embedded_daemon: DEFAULT_EMBEDDED_DAEMON,
            max_concurrent_agents: DEFAULT_MAX_CONCURRENT_AGENTS,
            shutdown_grace_ms: DEFAULT_SHUTDOWN_GRACE_MS,
            orphan_on_parent_death: DEFAULT_ORPHAN_ON_PARENT_DEATH,
            default_mode: DEFAULT_FLEET_MODE,
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] when one of the numeric
    /// overrides (`AURA_FLEET_MAX_CONCURRENT_AGENTS`,
    /// `AURA_FLEET_SHUTDOWN_GRACE_MS`) is non-empty but unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        cfg.embedded_daemon = lookup_bool(
            AURA_FLEET_EMBEDDED_DAEMON,
            DEFAULT_EMBEDDED_DAEMON,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        if let Some(v) = lookup_numeric::<u32>(AURA_FLEET_MAX_CONCURRENT_AGENTS)? {
            cfg.max_concurrent_agents = v;
        }
        if let Some(v) = lookup_numeric::<u64>(AURA_FLEET_SHUTDOWN_GRACE_MS)? {
            cfg.shutdown_grace_ms = v;
        }
        cfg.orphan_on_parent_death = lookup_bool(
            AURA_FLEET_ORPHAN_ON_PARENT_DEATH,
            DEFAULT_ORPHAN_ON_PARENT_DEATH,
            TRUTHY_LITERALS,
            FALSY_LITERALS,
        );
        if let Some(raw) = std::env::var(AURA_FLEET_DEFAULT_MODE)
            .ok()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
        {
            cfg.default_mode = match raw.as_str() {
                "agent" => AgentMode::Agent,
                "plan" => AgentMode::Plan,
                "ask" => AgentMode::Ask,
                "debug" => AgentMode::Debug,
                _ => {
                    return Err(crate::ConfigError::InvalidValue {
                        name: AURA_FLEET_DEFAULT_MODE,
                        raw,
                        message: "expected one of agent|plan|ask|debug".to_string(),
                    });
                }
            };
        }
        Ok(cfg)
    }
}

impl Default for FleetConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ENV_TEST_LOCK;

    fn clear_fleet_env() {
        std::env::remove_var(AURA_FLEET_EMBEDDED_DAEMON);
        std::env::remove_var(AURA_FLEET_MAX_CONCURRENT_AGENTS);
        std::env::remove_var(AURA_FLEET_SHUTDOWN_GRACE_MS);
        std::env::remove_var(AURA_FLEET_ORPHAN_ON_PARENT_DEATH);
        std::env::remove_var(AURA_FLEET_DEFAULT_MODE);
    }

    #[test]
    fn defaults_are_stable() {
        let cfg = FleetConfig::defaults();
        assert!(cfg.embedded_daemon);
        assert_eq!(cfg.max_concurrent_agents, DEFAULT_MAX_CONCURRENT_AGENTS);
        assert_eq!(cfg.shutdown_grace_ms, DEFAULT_SHUTDOWN_GRACE_MS);
        assert!(cfg.orphan_on_parent_death);
        assert_eq!(cfg.default_mode, AgentMode::Agent);
    }

    #[test]
    fn from_env_parses_default_mode() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_fleet_env();
        std::env::set_var(AURA_FLEET_DEFAULT_MODE, "plan");
        let cfg = FleetConfig::from_env().expect("plan parses");
        assert_eq!(cfg.default_mode, AgentMode::Plan);
        clear_fleet_env();
    }

    #[test]
    fn from_env_rejects_unknown_default_mode() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_fleet_env();
        std::env::set_var(AURA_FLEET_DEFAULT_MODE, "yolo");
        let err = FleetConfig::from_env().expect_err("yolo is not a known mode");
        assert!(matches!(err, crate::ConfigError::InvalidValue { .. }));
        clear_fleet_env();
    }

    #[test]
    fn defaults_are_const_evaluable() {
        const _DEFAULTS: FleetConfig = FleetConfig::defaults();
    }

    #[test]
    fn from_env_falls_back_to_defaults_when_unset() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_fleet_env();
        let cfg = FleetConfig::from_env().expect("defaults must parse");
        assert_eq!(cfg, FleetConfig::defaults());
        clear_fleet_env();
    }

    #[test]
    fn from_env_applies_numeric_overrides() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_fleet_env();
        std::env::set_var(AURA_FLEET_MAX_CONCURRENT_AGENTS, "7");
        std::env::set_var(AURA_FLEET_SHUTDOWN_GRACE_MS, "1234");
        let cfg = FleetConfig::from_env().expect("override must parse");
        assert_eq!(cfg.max_concurrent_agents, 7);
        assert_eq!(cfg.shutdown_grace_ms, 1234);
        clear_fleet_env();
    }

    #[test]
    fn from_env_applies_bool_overrides() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_fleet_env();
        std::env::set_var(AURA_FLEET_EMBEDDED_DAEMON, "false");
        std::env::set_var(AURA_FLEET_ORPHAN_ON_PARENT_DEATH, "no");
        let cfg = FleetConfig::from_env().expect("override must parse");
        assert!(!cfg.embedded_daemon);
        assert!(!cfg.orphan_on_parent_death);
        clear_fleet_env();
    }
}
