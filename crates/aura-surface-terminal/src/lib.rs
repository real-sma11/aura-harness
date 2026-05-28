//! # aura-surface-terminal
//!
//! Layer: surface
//!
//! Phase 9 relocation shell for the Aura TUI. The underlying TUI
//! implementation continues to live in the legacy `aura-terminal`
//! crate; this surface-layer crate re-exports it so the layered
//! `aura-<layer>-<name>` convention applies to the binary front-end
//! the same way it applies to `aura-fleet-*` and `aura-agent-*`
//! crates.
//!
//! Code that was previously written against `aura_terminal::*` may
//! import from `aura_surface_terminal::*` instead; both paths are
//! source-compatible while the migration completes.
//!
//! ## Invariants ([`.cursor/rules.md`] §13)
//!
//! - This crate adds no runtime behaviour. It exists to make the
//!   workspace `Cargo.toml` topology match the documented layer
//!   stack. The single semantic addition is [`SlashModeCommand`],
//!   the typed `/mode` slash-command shape used at session start to
//!   feed the [`aura_core_modes::AgentMode`] resolution priority.
//! - No upward dependency on `aura-runtime` or `aura-fleet-*`. The
//!   TUI talks to a fleet daemon through the surface-layer
//!   composition root (`aura-surface-cli`), never directly.
//!
//! ## Failure modes
//!
//! - [`ModeCommandError::UnknownMode`] — operator typed `/mode foo`
//!   with `foo` not in {`agent`, `plan`, `ask`, `debug`}.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_terminal::*;

use aura_core_modes::AgentMode;
use thiserror::Error;

/// Errors raised when parsing a `/mode <name>` slash command.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModeCommandError {
    /// The slash command body did not match any known [`AgentMode`].
    #[error("unknown mode `{0}`; expected one of agent|plan|ask|debug")]
    UnknownMode(String),
    /// The slash command had no argument.
    #[error("`/mode` requires a mode name; expected one of agent|plan|ask|debug")]
    MissingArgument,
}

/// Parsed `/mode <name>` slash-command payload.
///
/// Surfaced from the TUI input layer to the surface-layer
/// composition root so the resolution priority described in the
/// Phase 9 plan can be applied uniformly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashModeCommand {
    /// The parsed mode the operator typed.
    pub requested: AgentMode,
}

impl SlashModeCommand {
    /// Parse the body of a `/mode <name>` line.
    ///
    /// Accepts the lower-snake names that match the
    /// [`AgentMode`] serde representation. Whitespace around the
    /// argument is trimmed.
    ///
    /// # Errors
    ///
    /// Returns [`ModeCommandError::MissingArgument`] when the
    /// argument is empty after trimming and
    /// [`ModeCommandError::UnknownMode`] for any other unknown
    /// value.
    pub fn parse(arg: &str) -> Result<Self, ModeCommandError> {
        let trimmed = arg.trim();
        if trimmed.is_empty() {
            return Err(ModeCommandError::MissingArgument);
        }
        let mode = match trimmed {
            "agent" => AgentMode::Agent,
            "plan" => AgentMode::Plan,
            "ask" => AgentMode::Ask,
            "debug" => AgentMode::Debug,
            other => return Err(ModeCommandError::UnknownMode(other.to_string())),
        };
        Ok(Self { requested: mode })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_four_modes() {
        assert_eq!(
            SlashModeCommand::parse("agent").unwrap().requested,
            AgentMode::Agent
        );
        assert_eq!(
            SlashModeCommand::parse(" plan ").unwrap().requested,
            AgentMode::Plan
        );
        assert_eq!(
            SlashModeCommand::parse("ask").unwrap().requested,
            AgentMode::Ask
        );
        assert_eq!(
            SlashModeCommand::parse("debug").unwrap().requested,
            AgentMode::Debug
        );
    }

    #[test]
    fn rejects_unknown_mode() {
        let err = SlashModeCommand::parse("yolo").unwrap_err();
        assert!(matches!(err, ModeCommandError::UnknownMode(s) if s == "yolo"));
    }

    #[test]
    fn rejects_empty_argument() {
        assert!(matches!(
            SlashModeCommand::parse("   "),
            Err(ModeCommandError::MissingArgument)
        ));
    }
}
