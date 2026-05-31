//! # aura-core-types
//!
//! Layer: core
//!
//! Behavior-free IDs and lightweight shared shapes for the new
//! layered architecture.
//!
//! ## Phase 1 scope
//!
//! Phase 1 lands the following NEW identifier newtypes here:
//! [`TurnId`], [`RunId`], [`ToolCallId`], [`UserId`], [`SessionId`],
//! and [`TransactionId`].
//!
//! The legacy identifiers (`AgentId`, `Hash`, `ActionId`,
//! `ProcessId`, etc.) and the larger domain types
//! (`Transaction`, `Action`, `Effect`, `RecordEntry`,
//! `SpawnSpec`, `SubagentResult`, …) still live in `aura-core` and
//! are re-exported through it. They migrate here in Phase 1.5 / Phase
//! 2 as the rest of the workspace is rewired off `aura-core`
//! directly. Re-exporting in the other direction in Phase 1 would
//! create a dependency cycle because `aura-core` consumes the
//! permissions and modes crates.
//!
//! ## Invariants
//!
//! - Every id is a fixed-size byte newtype with hex display.
//! - `from_uuid` and `generate` always produce deterministic byte
//!   representations.
//!
//! ## Failure modes
//!
//! - [`hex::FromHexError`] when `from_hex` is called with malformed
//!   input.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod error;
pub mod hash;
mod ids;
mod registry;
mod serde_helpers;
mod time;
mod types;

pub use ids::{RunId, SessionId, ToolCallId, TransactionId, TurnId, UserId};

pub use error::{AuraError, Result};
#[allow(deprecated)]
pub use ids::{ActionId, AgentEventId, AgentId, FactId, Hash, ProcedureId, ProcessId, TxId};
pub use registry::{Registry, RegistryError};
#[allow(deprecated)]
pub use types::ToolDecision;
pub use types::{
    installed_integrations_satisfy, integration_match, Action, ActionKind, ActionResultPayload,
    AgentStatus, CacheControl, ContextHash, Decision, Effect, EffectKind, EffectStatus, Identity,
    InstalledIntegrationDefinition, InstalledToolCapability, InstalledToolDefinition,
    InstalledToolIntegrationRequirement, InstalledToolRuntimeAuth, InstalledToolRuntimeExecution,
    InstalledToolRuntimeIntegration, InstalledToolRuntimeProviderExecution, LineDiff,
    ProcessPending, Proposal, ProposalSet, RecordEntry, RecordEntryBuilder, RejectedProposal,
    RuntimeCapabilityInstall, SubagentBudget, SubagentDispatchRequest, SubagentExit,
    SubagentKindSpec, SubagentResult, SystemKind, ToolAuth, ToolCall, ToolCallContext,
    ToolDefinition, ToolExecution, ToolGateVerdict, ToolProposal, ToolResult, ToolResultContent,
    ToolResultKind, Trace, Transaction, TransactionType, DEFAULT_SUBAGENT_TIMEOUT_MS,
    KERNEL_VERSION, MAX_TURNS,
};

// Convenience re-exports of mode/permission primitives so downstream
// crates can pull from one place once they're migrated. These do not
// create cycles because aura-core-types depends on both.
pub use aura_core_modes::{
    AgentMode, AgentRole, CapabilityProfile, DefaultModeGate, GatedAction, JoinPolicy, KernelMode,
    ModeGate, ModeProfile, ModeViolation, ReplayMode, SandboxMode, SpawnMode,
};
pub use aura_core_permissions::{
    allows, allows_tool, effective, intersect, is_effectively_full_access, narrow,
    resolve_effective_permission, AgentPermissions, AgentScope, AgentToolPermissions, Capability,
    EffectivePermissions, GrantSource, PermissionDecision, PermissionError, Permissions,
    PrivilegeGrant, ToolState, UserDefaultMode, UserToolDefaults,
};
