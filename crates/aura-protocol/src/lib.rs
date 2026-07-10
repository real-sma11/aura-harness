//! Shared wire protocol types for the Aura harness HTTP + WebSocket API.
//!
//! Layer: core
//!
//! Defines the canonical request shape ([`RuntimeRequest`]) for the
//! `POST /v1/run` endpoint plus the inbound (client → server) and
//! outbound (server → client) message format for the per-run
//! `WS /stream/:run_id` WebSocket endpoint.
//!
//! This crate is consumed by both the harness server (`aura-node`)
//! and any client implementation (e.g. `aura-os-harness`). External
//! consumers interact with the harness exclusively over the wire
//! (HTTP + WS), so this crate is the single source of truth for the
//! wire-level vocabulary.
//!
//! # Module layout
//!
//! - [`runtime_request`]: canonical `POST /v1/run` body.
//! - [`agent_identity`]: [`AgentPersona`] persona bundle nested
//!   inside [`runtime_request::AgentIdentity`].
//! - [`client`]: inbound (client → server) WS envelope and payloads.
//! - [`server`]: outbound (server → client) WS envelope and payloads.
//! - [`context`]: rendered per-bucket context contents emitted
//!   alongside the [`server::ContextBreakdown`] token counts.
//! - [`common`]: small enums shared across both directions.
//! - [`permissions`]: wire-compatible mirrors of the harness
//!   agent-permission model.
//! - [`installed`]: installed-tool / installed-integration
//!   definitions.
//! - [`conversions`]: wire ↔ core helpers consumed by the harness's
//!   `aura-runtime` gateway.
//!
//! Every public type is re-exported at the crate root so existing
//! call sites continue to resolve.
//!
//! # Agent permissions model
//!
//! [`RuntimeRequest::agent_permissions`] is **required** on every
//! run. The harness enforces these permissions unconditionally —
//! there is no role-based fallback, no named preset, and no legacy
//! "no-permissions" default. Every caller submitting a run must
//! send an explicit [`AgentPermissionsWire`] value describing the
//! scope + capability bundle the run is allowed to exercise.

pub mod agent_identity;
pub mod chat_project_info;
pub mod client;
pub mod common;
pub mod context;
pub mod conversions;
pub mod installed;
pub mod permissions;
pub mod runtime_request;
pub mod server;

pub use agent_identity::AgentPersona;
pub use chat_project_info::ChatProjectInfoWire;
pub use client::{
    clamp_prompt_cache_key, ApprovalResponse, ConversationMessage, GenerationRequest,
    InboundMessage, IntentClassifierRule, IntentClassifierSpec, MessageAttachment,
    SessionModelOverrides, ToolApprovalResponse, UserMessage, MAX_PROMPT_CACHE_KEY_LEN,
};
pub use common::{ToolApprovalDecision, ToolApprovalRemember, ToolStateWire};
pub use context::{ContextContents, ContextSegment};
pub use conversions::{
    agent_tool_permissions_from_wire, installed_integration_to_core, installed_tool_to_core,
    tool_state_from_wire, tool_state_to_wire,
};
pub use installed::{
    InstalledIntegration, InstalledTool, InstalledToolIntegrationRequirement,
    InstalledToolRuntimeAuth, InstalledToolRuntimeExecution, InstalledToolRuntimeIntegration,
    InstalledToolRuntimeProviderExecution, ToolAuth,
};
pub use permissions::{
    AgentPermissionsWire, AgentScopeWire, AgentToolPermissionsWire, CapabilityWire,
};
pub use runtime_request::{
    AgentCapabilities, AgentIdentity, CouncilMechanism, CouncilMember, CouncilMemberRole,
    ModelSelection, ProjectContext, ReasoningEffort, RuntimeRequest, RuntimeRequestType,
    RuntimeRunResponse, WorkspaceLocation,
};
pub use server::{
    AssistantMessageEnd, AssistantMessageStart, ContextBreakdown, ErrorMsg, FileDiff, FileOp,
    FilesChanged, GenerationCompleted, GenerationErrorMsg, GenerationPartialImage,
    GenerationProgressMsg, GenerationStart, OutboundMessage, ProgressMsg, SessionReady,
    SessionUsage, SkillInfo, SubagentSpawned, SubagentStatus, TextDelta, ThinkingDelta,
    ToolApprovalPrompt, ToolCallSnapshot, ToolInfo, ToolResultMsg, ToolUseStart,
};

#[cfg(all(test, feature = "typescript"))]
mod ts_export {
    use super::{InboundMessage, OutboundMessage, RuntimeRequest, RuntimeRunResponse};
    use ts_rs::TS;

    #[test]
    fn export_typescript_bindings() {
        InboundMessage::export_all().unwrap();
        OutboundMessage::export_all().unwrap();
        RuntimeRequest::export_all().unwrap();
        RuntimeRunResponse::export_all().unwrap();
    }
}
