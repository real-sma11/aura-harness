//! # aura-core
//!
//! Core types, identifiers, schemas, and serialization for Aura.
//!
//! This crate provides:
//! - Strongly-typed identifiers (`AgentId`, `TxId`, `ActionId`, `Hash`, `ProcessId`)
//! - Domain types (`Transaction`, `Action`, `Effect`, `RecordEntry`)
//! - Async process types (`ProcessPending`, `ActionResultPayload`)
//! - Error types
//! - Hashing utilities

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub(crate) mod error;
pub mod hash;
pub(crate) mod ids;
pub(crate) mod permissions;
pub(crate) mod registry;
pub(crate) mod serde_helpers;
pub(crate) mod time;
pub(crate) mod types;

pub use error::{AuraError, Result};
#[allow(deprecated)]
pub use ids::{ActionId, AgentEventId, AgentId, FactId, Hash, ProcedureId, ProcessId, TxId};
pub use permissions::{AgentPermissions, AgentScope, Capability};
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
    ProcessPending, Proposal, ProposalSet, RecordEntry, RejectedProposal, RuntimeCapabilityInstall,
    SubagentBudget, SubagentDispatchRequest, SubagentExit, SubagentKindSpec, SubagentResult,
    SystemKind, ToolAuth, ToolCall, ToolCallContext, ToolDefinition, ToolExecution,
    ToolGateVerdict, ToolProposal, ToolResult, ToolResultContent, ToolResultKind, ToolState, Trace,
    Transaction, TransactionType, UserDefaultMode, UserToolDefaults, DEFAULT_SUBAGENT_TIMEOUT_MS,
};
