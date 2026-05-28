//! Wire ↔ core conversion helpers.
//!
//! Moved from `aura-runtime::protocol` in Phase A of the gateway
//! refactor so `aura-protocol` is the single canonical wire↔core
//! seam.
//!
//! The functions here turn the wire-shape mirrors carried on a
//! [`crate::RuntimeRequest`] into the `aura-core` types every
//! harness-internal consumer (kernel policy, tool resolver, executor
//! router) actually speaks. Both crates sit in the `core` layer so
//! this is a same-layer dependency edge.

use aura_core::{
    AgentToolPermissions, InstalledIntegrationDefinition, InstalledToolDefinition, ToolState,
};

use crate::common::ToolStateWire;
use crate::installed::{
    InstalledIntegration, InstalledTool, InstalledToolRuntimeAuth, InstalledToolRuntimeExecution,
    ToolAuth,
};
use crate::permissions::AgentToolPermissionsWire;

/// Convert a tri-state tool permission wire value into the
/// in-process [`ToolState`].
#[must_use]
pub fn tool_state_from_wire(state: ToolStateWire) -> ToolState {
    match state {
        ToolStateWire::On => ToolState::Allow,
        ToolStateWire::Off => ToolState::Deny,
        ToolStateWire::Ask => ToolState::Ask,
    }
}

/// Inverse of [`tool_state_from_wire`] — convert an in-process
/// [`ToolState`] back into its [`ToolStateWire`] mirror.
#[must_use]
pub fn tool_state_to_wire(state: ToolState) -> ToolStateWire {
    match state {
        ToolState::Allow => ToolStateWire::On,
        ToolState::Deny => ToolStateWire::Off,
        ToolState::Ask => ToolStateWire::Ask,
    }
}

/// Convert wire-side [`AgentToolPermissionsWire`] into the
/// harness-core [`AgentToolPermissions`].
#[must_use]
pub fn agent_tool_permissions_from_wire(wire: AgentToolPermissionsWire) -> AgentToolPermissions {
    AgentToolPermissions {
        per_tool: wire
            .per_tool
            .into_iter()
            .map(|(name, state)| (name, tool_state_from_wire(state)))
            .collect(),
    }
}

/// Convert a protocol [`InstalledTool`] into a core
/// [`InstalledToolDefinition`].
#[must_use]
pub fn installed_tool_to_core(t: InstalledTool) -> InstalledToolDefinition {
    InstalledToolDefinition {
        name: t.name,
        description: t.description,
        input_schema: t.input_schema,
        endpoint: t.endpoint,
        auth: match t.auth {
            ToolAuth::None => aura_core::ToolAuth::None,
            ToolAuth::Bearer { token } => aura_core::ToolAuth::Bearer { token },
            ToolAuth::ApiKey { header, key } => aura_core::ToolAuth::ApiKey { header, key },
            ToolAuth::Headers { headers } => aura_core::ToolAuth::Headers { headers },
        },
        timeout_ms: t.timeout_ms,
        namespace: t.namespace,
        required_integration: t.required_integration.map(|requirement| {
            aura_core::InstalledToolIntegrationRequirement {
                integration_id: requirement.integration_id,
                provider: requirement.provider,
                kind: requirement.kind,
            }
        }),
        runtime_execution: t.runtime_execution.map(|execution| match execution {
            InstalledToolRuntimeExecution::AppProvider(provider) => {
                aura_core::InstalledToolRuntimeExecution::AppProvider(
                    aura_core::InstalledToolRuntimeProviderExecution {
                        provider: provider.provider,
                        base_url: provider.base_url,
                        static_headers: provider.static_headers,
                        integrations: provider
                            .integrations
                            .into_iter()
                            .map(|integration| aura_core::InstalledToolRuntimeIntegration {
                                integration_id: integration.integration_id,
                                base_url: integration.base_url,
                                auth: match integration.auth {
                                    InstalledToolRuntimeAuth::None => {
                                        aura_core::InstalledToolRuntimeAuth::None
                                    }
                                    InstalledToolRuntimeAuth::AuthorizationBearer { token } => {
                                        aura_core::InstalledToolRuntimeAuth::AuthorizationBearer {
                                            token,
                                        }
                                    }
                                    InstalledToolRuntimeAuth::AuthorizationRaw { value } => {
                                        aura_core::InstalledToolRuntimeAuth::AuthorizationRaw {
                                            value,
                                        }
                                    }
                                    InstalledToolRuntimeAuth::Header { name, value } => {
                                        aura_core::InstalledToolRuntimeAuth::Header { name, value }
                                    }
                                    InstalledToolRuntimeAuth::QueryParam { name, value } => {
                                        aura_core::InstalledToolRuntimeAuth::QueryParam {
                                            name,
                                            value,
                                        }
                                    }
                                    InstalledToolRuntimeAuth::Basic { username, password } => {
                                        aura_core::InstalledToolRuntimeAuth::Basic {
                                            username,
                                            password,
                                        }
                                    }
                                },
                                provider_config: integration.provider_config,
                            })
                            .collect(),
                    },
                )
            }
        }),
        metadata: t.metadata,
    }
}

/// Convert a protocol [`InstalledIntegration`] into a core
/// [`InstalledIntegrationDefinition`].
#[must_use]
pub fn installed_integration_to_core(
    integration: InstalledIntegration,
) -> InstalledIntegrationDefinition {
    InstalledIntegrationDefinition {
        integration_id: integration.integration_id,
        name: integration.name,
        provider: integration.provider,
        kind: integration.kind,
        metadata: integration.metadata,
    }
}
