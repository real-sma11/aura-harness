//! Privilege grants and their originating sources.

use serde::{Deserialize, Serialize};

use crate::capability::Capability;

/// Origin of a [`PrivilegeGrant`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantSource {
    /// Granted explicitly by the user.
    User,
    /// Contributed by an installed plugin.
    Plugin {
        /// Identifier of the plugin that supplied the grant.
        plugin_id: String,
    },
    /// Inherited from a parent agent.
    Inherited,
}

/// A single capability grant along with its provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivilegeGrant {
    /// Where this grant came from.
    pub source: GrantSource,
    /// The capability granted.
    pub capability: Capability,
}
