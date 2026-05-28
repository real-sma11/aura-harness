//! # aura-surface-cli
//!
//! Layer: surface
//!
//! Phase 9 surface-layer composition root for the `aura` and
//! `aura-node` binaries. The new architecture's intent is that
//! the root binaries are thin entry-points that delegate to this
//! crate, and that this crate is the only place where CLI parsing,
//! session bootstrapping, and daemon-client wiring live.
//!
//! Phase 9 introduces the public *type surface* of the composition
//! root (CLI flag definitions, `AgentMode` resolution helper) while
//! preserving the existing root-binary behaviour. The full
//! migration of `src/main.rs` (the `aura` binary) and
//! `crates/aura-runtime/src/main.rs` (the `aura-node` binary) into
//! this crate proceeds incrementally so the CLI golden tests
//! remain byte-identical across the rename.
//!
//! ## Mode resolution priority
//!
//! Phase 9 wires four input rungs:
//!
//! 1. **CLI flag** — `aura --mode <agent|plan|ask|debug>` and the
//!    per-subcommand variants. The [`ModeFlag`] type below is the
//!    `clap`-derived shape.
//! 2. **TUI slash command** — `/mode <agent|plan|ask|debug>` from
//!    inside `aura-surface-terminal`. Parsed by
//!    [`aura_surface_terminal::SlashModeCommand`].
//! 3. **SDK field** — [`aura_surface_sdk::SessionConfig::mode`].
//! 4. **Daemon default** — [`aura_config::FleetConfig::default_mode`].
//! 5. **Fallback** — [`AgentMode::Agent`].
//!
//! The actual resolution math (lowest → highest precedence) lives in
//! `aura_fleet_daemon::resolve_session_mode` so child agents can be
//! propagated identically. This crate only exposes the surface-side
//! input plumbing.
//!
//! ## Invariants ([`.cursor/rules.md`] §13)
//!
//! - No upward dependency on `aura-runtime` or `aura-fleet-*`. The
//!   composition root sits at the surface layer and below.
//! - No `anyhow` in any function signature exposed to library
//!   consumers; the root binaries continue to use `anyhow` in their
//!   `main` only.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use aura_core_modes::AgentMode;
use clap::ValueEnum;
use thiserror::Error;

/// `clap`-friendly mirror of [`AgentMode`].
///
/// Identical variants and serde representation; lives here so the
/// `aura --mode <agent|plan|ask|debug>` CLI surface stays a thin
/// surface-layer concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lower")]
pub enum ModeFlag {
    /// Full unrestricted operation subject to user permissions.
    Agent,
    /// Read + markdown writes + read-only subprocesses.
    Plan,
    /// Read-only.
    Ask,
    /// Read + sandboxed probes only.
    Debug,
}

impl From<ModeFlag> for AgentMode {
    fn from(value: ModeFlag) -> Self {
        match value {
            ModeFlag::Agent => AgentMode::Agent,
            ModeFlag::Plan => AgentMode::Plan,
            ModeFlag::Ask => AgentMode::Ask,
            ModeFlag::Debug => AgentMode::Debug,
        }
    }
}

impl From<AgentMode> for ModeFlag {
    fn from(value: AgentMode) -> Self {
        match value {
            AgentMode::Agent => ModeFlag::Agent,
            AgentMode::Plan => ModeFlag::Plan,
            AgentMode::Ask => ModeFlag::Ask,
            AgentMode::Debug => ModeFlag::Debug,
        }
    }
}

/// Errors raised by surface-layer CLI utilities.
#[derive(Debug, Error)]
pub enum CliError {
    /// The supplied CLI argument did not parse as an
    /// [`AgentMode`]. The string is the offending input.
    #[error("invalid --mode value `{0}`; expected one of agent|plan|ask|debug")]
    InvalidMode(String),
}

/// Parse a free-form `--mode <name>` argument into an
/// [`AgentMode`].
///
/// Useful when wiring older subcommands that already take a
/// `String` and need a small upgrade path.
///
/// # Errors
///
/// Returns [`CliError::InvalidMode`] on any value not in
/// `{agent, plan, ask, debug}`.
pub fn parse_mode_str(value: &str) -> Result<AgentMode, CliError> {
    let trimmed = value.trim();
    match trimmed {
        "agent" => Ok(AgentMode::Agent),
        "plan" => Ok(AgentMode::Plan),
        "ask" => Ok(AgentMode::Ask),
        "debug" => Ok(AgentMode::Debug),
        other => Err(CliError::InvalidMode(other.to_string())),
    }
}

/// Bundle of CLI inputs that influence the AgentMode resolution
/// priority. Phase 9 surfaces this struct so callers can pass a
/// single typed value into the daemon's resolver.
#[derive(Debug, Clone, Copy, Default)]
pub struct CliModeInputs {
    /// `--mode <agent|plan|ask|debug>` parsed from clap.
    pub cli_flag: Option<AgentMode>,
}

impl CliModeInputs {
    /// Construct from an optional [`ModeFlag`] (the clap-parsed
    /// `Option<ModeFlag>`).
    #[must_use]
    pub fn from_flag(flag: Option<ModeFlag>) -> Self {
        Self {
            cli_flag: flag.map(AgentMode::from),
        }
    }
}

/// Documented entry point of the surface-layer composition root.
///
/// Phase 9 ships a stub so the `aura` binary can call into this
/// crate without depending on its still-monolithic
/// implementation. Phase 10 migrates the body of `src/main.rs`
/// into here.
///
/// # Errors
///
/// Returns [`CliError`] for surface-layer argument-parsing
/// failures; the binary's `main` decides whether to bubble up
/// other (e.g. async / I/O) errors as `anyhow::Error`.
pub fn version_banner() -> String {
    format!(
        "aura {VERSION} — Phase 9 surface composition root",
        VERSION = env!("CARGO_PKG_VERSION")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_flag_roundtrips_to_agent_mode() {
        for mode in [
            AgentMode::Agent,
            AgentMode::Plan,
            AgentMode::Ask,
            AgentMode::Debug,
        ] {
            let flag: ModeFlag = mode.into();
            let back: AgentMode = flag.into();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn parse_mode_str_accepts_known_values() {
        assert_eq!(parse_mode_str("agent").unwrap(), AgentMode::Agent);
        assert_eq!(parse_mode_str("plan").unwrap(), AgentMode::Plan);
        assert_eq!(parse_mode_str("ask").unwrap(), AgentMode::Ask);
        assert_eq!(parse_mode_str("debug").unwrap(), AgentMode::Debug);
    }

    #[test]
    fn parse_mode_str_rejects_unknown() {
        assert!(matches!(
            parse_mode_str("yolo"),
            Err(CliError::InvalidMode(_))
        ));
    }

    #[test]
    fn version_banner_contains_phase_marker() {
        assert!(version_banner().contains("Phase 9"));
    }
}
