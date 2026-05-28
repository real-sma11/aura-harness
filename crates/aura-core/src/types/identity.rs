//! Agent identity types.

use crate::ids::AgentId;
use crate::types::tool_permissions::AgentToolPermissions;
use aura_core_permissions::AgentPermissions;
use serde::{Deserialize, Serialize};

/// Agent identity information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Agent identifier
    pub agent_id: AgentId,
    /// ZNS identifier (e.g., "0://Agent09")
    pub zns_id: String,
    /// Mutable display name
    pub name: String,
    /// Fingerprint of the identity
    #[serde(with = "crate::serde_helpers::hex_bytes_32")]
    pub identity_hash: [u8; 32],
    /// Scope + capability bundle attached to this agent. Required on
    /// every `Identity`; there is no "legacy, unknown" fallback and no
    /// serde default. Use [`AgentPermissions::full_access`] for the default
    /// agent grant, or [`AgentPermissions::empty`] for an explicitly
    /// restricted agent with no grants.
    pub permissions: AgentPermissions,
    /// Optional per-tool permission override. `None` (or an empty map)
    /// means "inherit the originating user's default" — see
    /// [`crate::resolve_effective_permission`]. Populated entries override
    /// only that specific tool; anything unlisted still flows through the
    /// user default. Serde-default + skip_serializing_if so legacy
    /// identities without this field deserialize unchanged and untouched
    /// identities don't gain a new wire footprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_permissions: Option<AgentToolPermissions>,
}

impl Identity {
    /// Create a new identity with the default full-access permission bundle.
    /// Callers that need a narrower grant should chain
    /// [`Self::with_permissions`].
    #[must_use]
    pub fn new(zns_id: impl Into<String>, name: impl Into<String>) -> Self {
        let zns_id = zns_id.into();
        let name = name.into();

        let identity_hash = *blake3::hash(zns_id.as_bytes()).as_bytes();
        let agent_id = AgentId::new(identity_hash);

        Self {
            agent_id,
            zns_id,
            name,
            identity_hash,
            permissions: AgentPermissions::full_access(),
            tool_permissions: None,
        }
    }

    /// Replace this identity's [`AgentPermissions`].
    #[must_use]
    pub fn with_permissions(mut self, permissions: AgentPermissions) -> Self {
        self.permissions = permissions;
        self
    }

    /// Replace this identity's per-agent tool permission override. Pass
    /// `None` (or an empty [`AgentToolPermissions`]) to fall back to the
    /// user default for every tool.
    #[must_use]
    pub fn with_tool_permissions(mut self, tool_permissions: Option<AgentToolPermissions>) -> Self {
        self.tool_permissions = tool_permissions;
        self
    }
}
