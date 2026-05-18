//! Tool-related types: proposals, executions, definitions, calls, and results.
//!
//! The module is split across siblings for readability. Public items are
//! re-exported here so external callers (and the parent `types::mod`)
//! continue to import them via `aura_core::types::tool::Item` exactly
//! as before the Phase 2a split.
//!
//! - [`proposal`] — `ToolProposal`, `ToolGateVerdict` (audit-log enum;
//!   exported with a deprecated `ToolDecision` alias for back-compat).
//! - [`execution`] — `ToolExecution`, `ToolCallContext`.
//! - [`installed`] — `InstalledToolDefinition` and the catalog shapes.
//! - [`runtime_capability`] — `RuntimeCapabilityInstall` plus the
//!   canonical `integration_match` / `installed_integrations_satisfy`
//!   predicates.
//! - [`call`] — `ToolCall` envelope and credential-redaction `Debug` impl.
//! - [`result`] — `ToolResult` from tool execution.

mod call;
mod execution;
mod installed;
mod proposal;
mod result;
mod runtime_capability;

pub use call::ToolCall;
pub use execution::{ToolCallContext, ToolExecution};
pub use installed::{
    InstalledIntegrationDefinition, InstalledToolCapability, InstalledToolDefinition,
    InstalledToolIntegrationRequirement, InstalledToolRuntimeAuth, InstalledToolRuntimeExecution,
    InstalledToolRuntimeIntegration, InstalledToolRuntimeProviderExecution, ToolAuth,
};
#[allow(deprecated)]
pub use proposal::ToolDecision;
pub use proposal::{ToolGateVerdict, ToolProposal};
pub use result::{LineDiff, ToolResult, ToolResultKind};
pub use runtime_capability::{
    installed_integrations_satisfy, integration_match, RuntimeCapabilityInstall,
};
