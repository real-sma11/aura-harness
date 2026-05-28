//! Closed mode enums.
//!
//! # Invariants
//!
//! Every enum here is a CLOSED enum: adding a variant is a breaking
//! change. No `_` wildcard arms are permitted in this crate so the
//! compiler catches all missing cases when a new variant is added.

use serde::{Deserialize, Serialize};

use crate::capability_profile::CapabilityProfile;
use crate::profile::ModeProfile;

/// The headline gate consulted before every external effect.
///
/// Wire format is lower-snake (`agent`, `plan`, `ask`, `debug`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Full unrestricted operation subject to user permissions.
    #[default]
    Agent,
    /// Read + markdown-only writes + read-only subprocesses; no
    /// spawning, no non-markdown writes, no network.
    Plan,
    /// Read-only — no subprocesses, no writes, no network.
    Ask,
    /// Read + sandboxed probes only.
    Debug,
}

impl AgentMode {
    /// Default [`ModeProfile`] for this mode (kernel/sandbox/replay).
    #[must_use]
    pub fn default_profile(self) -> ModeProfile {
        let sandbox = match self {
            AgentMode::Agent => SandboxMode::Standard,
            AgentMode::Plan | AgentMode::Debug => SandboxMode::Strict,
            AgentMode::Ask => SandboxMode::ReadOnly,
        };
        ModeProfile {
            agent: self,
            kernel: Self::default_kernel_mode(AgentRole::Root),
            sandbox,
            replay: ReplayMode::default(),
        }
    }

    /// Default capability discriminant set for this mode.
    ///
    /// Mirrors the rule table in the crate-level docs:
    ///
    /// | Mode  | Allows                                                                          |
    /// |-------|---------------------------------------------------------------------------------|
    /// | Agent | every capability                                                                |
    /// | Plan  | spawn-agent withheld; only read + markdown write + read-only subprocess         |
    /// | Ask   | read + read-only tool invocations only                                          |
    /// | Debug | read + sandboxed probes only                                                    |
    #[must_use]
    pub fn default_capability_profile(self) -> CapabilityProfile {
        use CapabilityProfile as P;
        match self {
            AgentMode::Agent => P::agent_default(),
            AgentMode::Plan => P::plan_default(),
            AgentMode::Ask => P::ask_default(),
            AgentMode::Debug => P::debug_default(),
        }
    }

    /// Default [`KernelMode`] for an agent of the given role.
    ///
    /// Root agents get `Audited`; children get `AuditedLite`.
    #[must_use]
    pub fn default_kernel_mode(role: AgentRole) -> KernelMode {
        match role {
            AgentRole::Root => KernelMode::Audited,
            AgentRole::Child => KernelMode::AuditedLite,
        }
    }

    /// True iff this mode allows spawning subagents.
    ///
    /// Only [`AgentMode::Agent`] permits spawning.
    #[must_use]
    pub fn allows_spawn(self) -> bool {
        match self {
            AgentMode::Agent => true,
            AgentMode::Plan | AgentMode::Ask | AgentMode::Debug => false,
        }
    }
}

/// Audit-payload detail.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum KernelMode {
    /// Full payload recording (root agents).
    #[default]
    Audited,
    /// Summary recording (`head + tail + full_hash`) above a size
    /// threshold; metadata, hashes, verdicts, and child attribution
    /// remain full-fidelity.
    AuditedLite,
}

/// Subagent dispatch mode.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SpawnMode {
    /// Parent awaits the child's completion before continuing the
    /// current turn.
    #[default]
    Wait,
    /// Child runs in the background; parent receives a handle
    /// immediately. Lifetime tied to the parent session.
    Detached,
    /// Background and decoupled from parent lifetime — survives
    /// parent termination, suitable for long-running batch jobs.
    Batch,
}

/// Join semantics for `task_group`-style multi-child dispatch.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum JoinPolicy {
    /// Wait for every child to finish.
    #[default]
    All,
    /// Wait for the first success; cancel the rest.
    Any,
    /// Fire-and-forget; do not wait, do not propagate cancellation.
    Abandon,
}

/// Kernel replay state.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayMode {
    /// Live execution — model and executor are real providers.
    #[default]
    Live,
    /// Replay from a historical sequence number — model and executor
    /// are shimmed from the audit log.
    Replay {
        /// Starting sequence number (inclusive) within the agent's
        /// audit record.
        from_seq: u64,
    },
}

/// Exec sandbox profile.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// No writes, no subprocess; reads only.
    ReadOnly,
    /// Standard write + subprocess allowed inside the workspace.
    #[default]
    Standard,
    /// Strict — read + markdown-only writes, no network, sandboxed
    /// subprocess only.
    Strict,
}

/// Whether the current agent is a root agent or a derived child.
///
/// Distinct from `SubagentKind` (a persona/role label) — this carries
/// only the structural relationship.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    /// Root (session-owning) agent.
    #[default]
    Root,
    /// Child / derived subagent.
    Child,
}
