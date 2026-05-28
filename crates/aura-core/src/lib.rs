//! # aura-core
//!
//! Layer: core (compatibility shell)
//!
//! Phase 1 compatibility shell. Permission, mode, and selected new
//! id primitives have moved to dedicated layered crates
//! (`aura-core-permissions`, `aura-core-modes`, `aura-core-types`)
//! and are re-exported here verbatim so existing call sites keep
//! compiling.
//!
//! Larger domain types (`Transaction`, `Action`, `Effect`,
//! `RecordEntry`, the legacy IDs `AgentId`/`Hash`/…, etc.) still
//! live in this crate and migrate to `aura-core-types` in a later
//! phase as the rest of the workspace is rewired.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub(crate) mod error;
pub mod hash;
pub(crate) mod ids;
pub(crate) mod registry;
pub(crate) mod serde_helpers;
pub(crate) mod time;
pub(crate) mod types;

pub use aura_core_modes::{AgentMode, KernelMode};
pub use aura_core_permissions::{AgentPermissions, AgentScope, Capability};
pub use error::{AuraError, Result};
#[allow(deprecated)]
pub use ids::{ActionId, AgentEventId, AgentId, FactId, Hash, ProcedureId, ProcessId, TxId};
pub use registry::{Registry, RegistryError};
#[allow(deprecated)]
pub use types::ToolDecision;
pub use types::{
    installed_integrations_satisfy, integration_match, is_effectively_full_access,
    resolve_effective_permission, Action, ActionKind, ActionResultPayload, AgentStatus,
    AgentToolPermissions, CacheControl, ContextHash, Decision, Effect, EffectKind, EffectStatus,
    Identity, InstalledIntegrationDefinition, InstalledToolCapability, InstalledToolDefinition,
    InstalledToolIntegrationRequirement, InstalledToolRuntimeAuth, InstalledToolRuntimeExecution,
    InstalledToolRuntimeIntegration, InstalledToolRuntimeProviderExecution, LineDiff,
    ProcessPending, Proposal, ProposalSet, RecordEntry, RecordEntryBuilder, RejectedProposal,
    RuntimeCapabilityInstall, SubagentBudget, SubagentDispatchRequest, SubagentExit,
    SubagentKindSpec, SubagentResult, SystemKind, ToolAuth, ToolCall, ToolCallContext,
    ToolDefinition, ToolExecution, ToolGateVerdict, ToolProposal, ToolResult, ToolResultContent,
    ToolResultKind, ToolState, Trace, Transaction, TransactionType, UserDefaultMode,
    UserToolDefaults, DEFAULT_SUBAGENT_TIMEOUT_MS, KERNEL_VERSION, MAX_TURNS,
};
