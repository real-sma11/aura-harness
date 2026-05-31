//! Installed tool / installed integration catalog shapes.
//!
//! These types describe what tools and integrations are *available* to a
//! runtime session. The kernel persists a sanitized snapshot of these via
//! [`super::runtime_capability::RuntimeCapabilityInstall`] so policy
//! checks can verify a tool's `required_integration` against what is
//! actually installed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Authentication configuration for installed tools.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[derive(Default)]
pub enum ToolAuth {
    #[default]
    None,
    Bearer {
        token: String,
    },
    ApiKey {
        header: String,
        key: String,
    },
    Headers {
        headers: HashMap<String, String>,
    },
}

/// Authentication material for provider execution owned by the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstalledToolRuntimeAuth {
    #[default]
    None,
    AuthorizationBearer {
        token: String,
    },
    AuthorizationRaw {
        value: String,
    },
    Header {
        name: String,
        value: String,
    },
    QueryParam {
        name: String,
        value: String,
    },
    Basic {
        username: String,
        password: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolRuntimeIntegration {
    pub integration_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default)]
    pub auth: InstalledToolRuntimeAuth,
    #[serde(default)]
    pub provider_config: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolRuntimeProviderExecution {
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub static_headers: HashMap<String, String>,
    #[serde(default)]
    pub integrations: Vec<InstalledToolRuntimeIntegration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstalledToolRuntimeExecution {
    AppProvider(InstalledToolRuntimeProviderExecution),
}

/// Definition for an installed tool (replaces `ExternalToolDefinition`).
///
/// Installed tools are dispatched via HTTP POST to an endpoint.
/// They can come from `tools.toml`, the HTTP install API, or `RuntimeRequest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolIntegrationRequirement {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub endpoint: String,
    #[serde(default)]
    pub auth: ToolAuth,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_integration: Option<InstalledToolIntegrationRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_execution: Option<InstalledToolRuntimeExecution>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Definition for an installed integration available to a runtime session.
///
/// Integrations are distinct from tools: an integration represents an
/// authorized external capability, while tools may depend on one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledIntegrationDefinition {
    pub integration_id: String,
    pub name: String,
    pub provider: String,
    pub kind: String,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Sanitized runtime-visible installed tool metadata for capability recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledToolCapability {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_integration: Option<InstalledToolIntegrationRequirement>,
}

impl From<&InstalledToolDefinition> for InstalledToolCapability {
    fn from(value: &InstalledToolDefinition) -> Self {
        Self {
            name: value.name.clone(),
            required_integration: value.required_integration.clone(),
        }
    }
}
