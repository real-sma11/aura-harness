//! Centralized env-var names and parsing primitives.
//!
//! This module owns every env-var **name constant** that
//! `aura-config` parses. The boundary test `tests/config_boundary.rs`
//! at the workspace root reads [`ENV_VAR_NAMES`] and asserts no crate
//! other than `aura-config` calls `std::env::var(...)` on any of these
//! names. Tests that need to mutate one of these vars must do so via
//! [`crate::install_for_test`] instead.

use std::env::VarError;
use std::str::FromStr;

use thiserror::Error;

// ---------------------------------------------------------------------------
// Owned env-var names.
//
// These constants are the SINGLE source of truth for the textual env-var
// names. Add a new owned var by appending it here AND including it in
// `ENV_VAR_NAMES`.
// ---------------------------------------------------------------------------

pub const AURA_AGENT_DISABLE_COMPACTION: &str = "AURA_AGENT_DISABLE_COMPACTION";
pub const AURA_AGENT_IMPLEMENT_NOW: &str = "AURA_AGENT_IMPLEMENT_NOW";
pub const AURA_AGENT_IMPLEMENT_NOW_THRESHOLD: &str = "AURA_AGENT_IMPLEMENT_NOW_THRESHOLD";
pub const AURA_AGENT_IMPLEMENT_NOW_BLOCK: &str = "AURA_AGENT_IMPLEMENT_NOW_BLOCK";
pub const AURA_AGENT_BOOTSTRAP_SPEC_BYTES: &str = "AURA_AGENT_BOOTSTRAP_SPEC_BYTES";
pub const AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES: &str = "AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES";
pub const AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS: &str = "AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS";
pub const AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS: &str = "AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS";
pub const AURA_DOD_TEST_COMMAND: &str = "AURA_DOD_TEST_COMMAND";
pub const AURA_SIMPLE_MODEL: &str = "AURA_SIMPLE_MODEL";
pub const AURA_LLM_MAX_RETRIES: &str = "AURA_LLM_MAX_RETRIES";
pub const AURA_LLM_BACKOFF_INITIAL_MS: &str = "AURA_LLM_BACKOFF_INITIAL_MS";
pub const AURA_LLM_BACKOFF_CAP_MS: &str = "AURA_LLM_BACKOFF_CAP_MS";
pub const AURA_DEV_LOOP_ENABLED_THINKING: &str = "AURA_DEV_LOOP_ENABLED_THINKING";

/// The full list of env-var names this crate owns. Boundary tests use
/// this to enforce that no other crate calls `std::env::var(...)` on
/// any of these names.
pub const ENV_VAR_NAMES: &[&str] = &[
    AURA_AGENT_DISABLE_COMPACTION,
    AURA_AGENT_IMPLEMENT_NOW,
    AURA_AGENT_IMPLEMENT_NOW_THRESHOLD,
    AURA_AGENT_IMPLEMENT_NOW_BLOCK,
    AURA_AGENT_BOOTSTRAP_SPEC_BYTES,
    AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES,
    AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS,
    AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS,
    AURA_DOD_TEST_COMMAND,
    AURA_SIMPLE_MODEL,
    AURA_LLM_MAX_RETRIES,
    AURA_LLM_BACKOFF_INITIAL_MS,
    AURA_LLM_BACKOFF_CAP_MS,
    AURA_DEV_LOOP_ENABLED_THINKING,
];

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Failure to parse a centralized env override.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The env var was set to a non-empty value that did not parse as
    /// the expected numeric / boolean shape.
    #[error("env var `{name}` has invalid value `{raw}`: {message}")]
    InvalidValue {
        /// Name of the env var (one of the constants in this module).
        name: &'static str,
        /// Raw value as read from the process environment.
        raw: String,
        /// Underlying parse error message.
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Parse helpers (each `lookup_*` returns `None` for unset / blank vars
// so the caller can fall through to its compile-time default).
// ---------------------------------------------------------------------------

fn raw_var(name: &'static str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(VarError::NotPresent | VarError::NotUnicode(_)) => None,
    }
}

/// Parse a numeric env var. Returns `None` for unset / blank vars.
pub(crate) fn lookup_numeric<T>(name: &'static str) -> Result<Option<T>, ConfigError>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = raw_var(name) else {
        return Ok(None);
    };
    raw.parse::<T>()
        .map(Some)
        .map_err(|e| ConfigError::InvalidValue {
            name,
            raw,
            message: e.to_string(),
        })
}

/// Parse a non-zero numeric env var. Returns `None` for unset / blank
/// vars or when the value is zero (so consumers default to compile-time
/// fallbacks instead of disabling the knob accidentally).
pub(crate) fn lookup_nonzero_usize(name: &'static str) -> Result<Option<usize>, ConfigError> {
    Ok(lookup_numeric::<usize>(name)?.filter(|v| *v > 0))
}

/// Lookup a string env var. Returns `None` for unset / blank vars.
pub(crate) fn lookup_string(name: &'static str) -> Option<String> {
    raw_var(name)
}

/// Parse a truthy env var with explicit truthy / falsy literals.
///
/// `truthy_default` is the value returned when the env var is unset.
/// `truthy_literals` and `falsy_literals` are matched case-insensitively
/// against the trimmed raw value; any other non-empty value falls back
/// to `truthy_default`.
pub(crate) fn lookup_bool(
    name: &'static str,
    truthy_default: bool,
    truthy_literals: &[&str],
    falsy_literals: &[&str],
) -> bool {
    let Some(raw) = raw_var(name) else {
        return truthy_default;
    };
    let lower = raw.to_ascii_lowercase();
    if truthy_literals
        .iter()
        .any(|lit| lit.eq_ignore_ascii_case(&lower))
    {
        true
    } else if falsy_literals
        .iter()
        .any(|lit| lit.eq_ignore_ascii_case(&lower))
    {
        false
    } else {
        truthy_default
    }
}

/// Truthy literals used by "on by default unless explicitly disabled"
/// knobs (e.g. `AURA_AGENT_IMPLEMENT_NOW_BLOCK`). Matches the previous
/// inline `matches!(... Ok("0") | Ok("false") | Ok("no") | Ok("off"))`
/// negation logic.
pub(crate) const TRUTHY_LITERALS: &[&str] = &["1", "true", "yes", "on"];
pub(crate) const FALSY_LITERALS: &[&str] = &["0", "false", "no", "off"];
