//! [`ModeGate`] trait + default impl + [`GatedAction`] taxonomy.
//!
//! # Invariants
//!
//! Every external effect dispatches off the current [`crate::AgentMode`]
//! BEFORE consulting user permissions. The gate is the outermost
//! wrapper; permission checks are inside.
//!
//! The rule table (from the crate-level docs):
//!
//! | Mode  | Allows                                                                       |
//! |-------|------------------------------------------------------------------------------|
//! | Agent | every action                                                                 |
//! | Plan  | `ReadFile`, `WriteMarkdown`, read-only `Subprocess`, `PluginActivate`, RO ToolInvoke |
//! | Ask   | `ReadFile`, RO `ToolInvoke`                                                  |
//! | Debug | `ReadFile`, `SandboxedProbe` only                                            |
//!
//! "Read-only `ToolInvoke`" is enforced by the per-tool allowlist
//! constructed by higher layers; this gate accepts an opaque tool name
//! and only checks whether the mode permits any tool invocation at
//! all.

use serde::{Deserialize, Serialize};

use crate::modes::AgentMode;
use crate::violation::ModeViolation;

/// The closed taxonomy of mode-gated action classes.
///
/// Every external effect in the runtime maps to exactly one variant.
/// Adding a variant is a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GatedAction {
    /// Read a file from the workspace.
    ReadFile,
    /// Write a markdown (`.md`) file.
    WriteMarkdown,
    /// Write a non-markdown file.
    WriteNonMarkdown,
    /// Execute a subprocess.
    Subprocess,
    /// Make a network call.
    Network,
    /// Spawn a subagent.
    SpawnAgent,
    /// Activate a plugin.
    PluginActivate,
    /// Invoke a named tool.
    ToolInvoke {
        /// Tool name (opaque — the catalog enforces the allowlist).
        name: String,
    },
    /// Run a sandboxed probe (the only effect allowed in Debug mode
    /// besides reads).
    SandboxedProbe,
}

/// The mode gate trait. Implementors decide whether `action` is
/// allowed in `mode`.
pub trait ModeGate {
    /// Returns `Ok(())` if the action is allowed; otherwise a
    /// [`ModeViolation`].
    ///
    /// # Errors
    ///
    /// Returns a [`ModeViolation`] when the action is disallowed by
    /// the current mode.
    fn check(&self, mode: AgentMode, action: GatedAction) -> Result<(), ModeViolation>;
}

/// Default gate implementing the rule table in the crate-level docs.
///
/// Zero-sized — clone freely.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultModeGate;

impl ModeGate for DefaultModeGate {
    fn check(&self, mode: AgentMode, action: GatedAction) -> Result<(), ModeViolation> {
        match (mode, action) {
            // Agent mode allows every action.
            (AgentMode::Agent, _) => Ok(()),

            // Reads are universal across non-Agent modes.
            (AgentMode::Plan | AgentMode::Ask | AgentMode::Debug, GatedAction::ReadFile) => Ok(()),

            // Plan: markdown writes + subprocess (read-only by allowlist) +
            // plugin activation + tool invoke.
            (AgentMode::Plan, GatedAction::WriteMarkdown) => Ok(()),
            (AgentMode::Plan, GatedAction::Subprocess) => Ok(()),
            (AgentMode::Plan, GatedAction::PluginActivate) => Ok(()),
            (AgentMode::Plan, GatedAction::ToolInvoke { .. }) => Ok(()),
            (AgentMode::Plan, GatedAction::WriteNonMarkdown) => {
                Err(ModeViolation::WriteNonMarkdownNotAllowed)
            }
            (AgentMode::Plan, GatedAction::Network) => Err(ModeViolation::NetworkNotAllowed),
            (AgentMode::Plan, GatedAction::SpawnAgent) => Err(ModeViolation::SpawnNotAllowed),
            (AgentMode::Plan, GatedAction::SandboxedProbe) => Ok(()),

            // Ask: tool invoke (read-only allowlist) only besides ReadFile.
            (AgentMode::Ask, GatedAction::ToolInvoke { .. }) => Ok(()),
            (AgentMode::Ask, GatedAction::WriteMarkdown | GatedAction::WriteNonMarkdown) => {
                Err(ModeViolation::WriteNotAllowed)
            }
            (AgentMode::Ask, GatedAction::Subprocess) => Err(ModeViolation::SubprocessNotAllowed),
            (AgentMode::Ask, GatedAction::Network) => Err(ModeViolation::NetworkNotAllowed),
            (AgentMode::Ask, GatedAction::SpawnAgent) => Err(ModeViolation::SpawnNotAllowed),
            (AgentMode::Ask, GatedAction::PluginActivate) => {
                Err(ModeViolation::PluginActivationNotAllowed)
            }
            (AgentMode::Ask, GatedAction::SandboxedProbe) => {
                Err(ModeViolation::NonSandboxedActionInDebug)
            }

            // Debug: ReadFile + SandboxedProbe only.
            (AgentMode::Debug, GatedAction::SandboxedProbe) => Ok(()),
            (AgentMode::Debug, GatedAction::WriteMarkdown | GatedAction::WriteNonMarkdown) => {
                Err(ModeViolation::WriteNotAllowed)
            }
            (AgentMode::Debug, GatedAction::Subprocess) => Err(ModeViolation::SubprocessNotAllowed),
            (AgentMode::Debug, GatedAction::Network) => Err(ModeViolation::NetworkNotAllowed),
            (AgentMode::Debug, GatedAction::SpawnAgent) => Err(ModeViolation::SpawnNotAllowed),
            (AgentMode::Debug, GatedAction::PluginActivate) => {
                Err(ModeViolation::PluginActivationNotAllowed)
            }
            (AgentMode::Debug, GatedAction::ToolInvoke { name }) => {
                Err(ModeViolation::ToolInvocationNotAllowed { tool: name })
            }
        }
    }
}

/// Blanket convenience impl so callers can do `mode.check(action)`.
impl ModeGate for AgentMode {
    fn check(&self, mode: AgentMode, action: GatedAction) -> Result<(), ModeViolation> {
        DefaultModeGate.check(mode, action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_modes() -> [AgentMode; 4] {
        [
            AgentMode::Agent,
            AgentMode::Plan,
            AgentMode::Ask,
            AgentMode::Debug,
        ]
    }

    fn all_actions() -> Vec<GatedAction> {
        vec![
            GatedAction::ReadFile,
            GatedAction::WriteMarkdown,
            GatedAction::WriteNonMarkdown,
            GatedAction::Subprocess,
            GatedAction::Network,
            GatedAction::SpawnAgent,
            GatedAction::PluginActivate,
            GatedAction::ToolInvoke {
                name: "read_file".into(),
            },
            GatedAction::SandboxedProbe,
        ]
    }

    #[test]
    fn agent_mode_allows_every_action() {
        let gate = DefaultModeGate;
        for action in all_actions() {
            assert!(
                gate.check(AgentMode::Agent, action.clone()).is_ok(),
                "agent should allow {action:?}"
            );
        }
    }

    #[test]
    fn plan_mode_rule_table() {
        let gate = DefaultModeGate;
        assert!(gate.check(AgentMode::Plan, GatedAction::ReadFile).is_ok());
        assert!(gate
            .check(AgentMode::Plan, GatedAction::WriteMarkdown)
            .is_ok());
        assert!(gate.check(AgentMode::Plan, GatedAction::Subprocess).is_ok());
        assert!(gate
            .check(AgentMode::Plan, GatedAction::PluginActivate)
            .is_ok());
        assert!(gate
            .check(
                AgentMode::Plan,
                GatedAction::ToolInvoke {
                    name: "read_file".into(),
                },
            )
            .is_ok());
        assert!(gate
            .check(AgentMode::Plan, GatedAction::SandboxedProbe)
            .is_ok());
        assert_eq!(
            gate.check(AgentMode::Plan, GatedAction::WriteNonMarkdown),
            Err(ModeViolation::WriteNonMarkdownNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Plan, GatedAction::Network),
            Err(ModeViolation::NetworkNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Plan, GatedAction::SpawnAgent),
            Err(ModeViolation::SpawnNotAllowed),
        );
    }

    #[test]
    fn ask_mode_rule_table() {
        let gate = DefaultModeGate;
        assert!(gate.check(AgentMode::Ask, GatedAction::ReadFile).is_ok());
        assert!(gate
            .check(
                AgentMode::Ask,
                GatedAction::ToolInvoke {
                    name: "read_file".into(),
                },
            )
            .is_ok());
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::WriteMarkdown),
            Err(ModeViolation::WriteNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::WriteNonMarkdown),
            Err(ModeViolation::WriteNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::Subprocess),
            Err(ModeViolation::SubprocessNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::Network),
            Err(ModeViolation::NetworkNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::SpawnAgent),
            Err(ModeViolation::SpawnNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Ask, GatedAction::PluginActivate),
            Err(ModeViolation::PluginActivationNotAllowed),
        );
    }

    #[test]
    fn debug_mode_rule_table() {
        let gate = DefaultModeGate;
        assert!(gate.check(AgentMode::Debug, GatedAction::ReadFile).is_ok());
        assert!(gate
            .check(AgentMode::Debug, GatedAction::SandboxedProbe)
            .is_ok());
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::WriteMarkdown),
            Err(ModeViolation::WriteNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::WriteNonMarkdown),
            Err(ModeViolation::WriteNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::Subprocess),
            Err(ModeViolation::SubprocessNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::Network),
            Err(ModeViolation::NetworkNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::SpawnAgent),
            Err(ModeViolation::SpawnNotAllowed),
        );
        assert_eq!(
            gate.check(AgentMode::Debug, GatedAction::PluginActivate),
            Err(ModeViolation::PluginActivationNotAllowed),
        );
        assert_eq!(
            gate.check(
                AgentMode::Debug,
                GatedAction::ToolInvoke {
                    name: "read_file".into(),
                },
            ),
            Err(ModeViolation::ToolInvocationNotAllowed {
                tool: "read_file".into(),
            }),
        );
    }

    #[test]
    fn every_action_decided_in_every_mode() {
        let gate = DefaultModeGate;
        for mode in all_modes() {
            for action in all_actions() {
                let _ = gate.check(mode, action);
            }
        }
    }

    #[test]
    fn blanket_impl_matches_default_gate() {
        let gate = DefaultModeGate;
        for mode in all_modes() {
            for action in all_actions() {
                assert_eq!(
                    ModeGate::check(&mode, mode, action.clone()),
                    gate.check(mode, action),
                );
            }
        }
    }

    #[test]
    fn allows_spawn_only_for_agent_mode() {
        assert!(AgentMode::Agent.allows_spawn());
        assert!(!AgentMode::Plan.allows_spawn());
        assert!(!AgentMode::Ask.allows_spawn());
        assert!(!AgentMode::Debug.allows_spawn());
    }

    #[test]
    fn closed_enum_serde_round_trip() {
        for mode in all_modes() {
            let json = serde_json::to_string(&mode).unwrap();
            let back: AgentMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, back);
        }

        for action in all_actions() {
            let json = serde_json::to_string(&action).unwrap();
            let back: GatedAction = serde_json::from_str(&json).unwrap();
            assert_eq!(action, back);
        }
    }
}
