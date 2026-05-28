//! # aura-surface-cli
//!
//! Layer: surface
//!
//! Phase 9 + 10 surface-layer composition root for the `aura` CLI
//! binary. The root `src/main.rs` entry point is a thin shim that
//! delegates to [`run`]; this crate owns CLI parsing, session
//! bootstrapping, plugin handling, the embedded TUI / API server,
//! and the daemon-client wiring.
//!
//! Phase 9 shipped the public *type surface* of the composition
//! root (CLI flag definitions, `AgentMode` resolution helper).
//! Phase 10 carve-out 1 lifts the full body of `src/main.rs` (the
//! `aura` binary) into this crate. The `aura-node` binary body lives
//! in [`aura_runtime::run_node`] because `aura-runtime` owns the
//! HTTP/WS gateway binary and `aura-surface-cli` already depends on
//! `aura-runtime` for headless-mode wiring; this crate re-exports it
//! as [`run_node`] so the documented surface path remains callable.
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
//! - No upward dependency above the surface layer.
//! - No `anyhow` in any function signature *exposed to library
//!   consumers*; [`run`] / [`run_node`] return `anyhow::Result`
//!   because they are the binary entry-points and the root
//!   `main.rs` files immediately bubble the error to the process
//!   exit code, matching the same boundary the previous root
//!   binaries had.

#![forbid(unsafe_code)]
#![warn(clippy::all)]
// Phase 10 carve-out 1: the binary body migration brings in a
// large block of legacy code that pre-dated the workspace's
// curated clippy set. The lints below are pragma-allowed for
// the migrated CLI scaffolding only; new code added to this
// crate is still subject to the workspace defaults.
#![allow(
    clippy::manual_let_else,
    clippy::map_unwrap_or,
    clippy::cast_possible_wrap,
    clippy::ref_option,
    clippy::needless_pass_by_value,
    clippy::unnecessary_wraps
)]

use aura_core_modes::AgentMode;
use clap::ValueEnum;
use thiserror::Error;

pub mod api_server;
pub mod cli;
pub mod event_loop;
pub mod record_loader;
pub mod session_helpers;

mod runner;

pub use runner::run;

/// Phase 10 carve-out 1: surface-layer re-export of the
/// `aura-node` entrypoint. The implementation lives in
/// `aura_runtime::run_node` to avoid a dependency cycle between
/// `aura-surface-cli` and `aura-runtime` (aura-runtime owns the
/// `aura-node` binary, and aura-surface-cli already depends on
/// aura-runtime for headless mode wiring). The re-export keeps
/// the documented Phase 10 acceptance path `aura_surface_cli::run_node`
/// callable.
pub use aura_runtime::run_node;

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

/// Pinned version banner string used by the Phase 9 surface-layer
/// smoke tests. Kept intentionally short so the snapshot pin is
/// trivial to maintain across Phase 10 changes.
#[must_use]
pub fn version_banner() -> String {
    format!(
        "aura {VERSION} — Phase 10 surface composition root",
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
        assert!(version_banner().contains("Phase 10"));
    }
}
