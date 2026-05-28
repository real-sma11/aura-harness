//! Wire-compatible agent persona bundle.
//!
//! Renders into the `<agent_identity>` section of the assembled
//! system prompt: name / role / personality. Lives in `aura-protocol`
//! so the aura-os producer side and the harness consumer side share
//! a single source of truth without either repo depending on the
//! other's domain crate.
//!
//! Renamed from `AgentIdentityWire` → `AgentPersona` in Phase A of
//! the gateway refactor to free up the `AgentIdentity` name for the
//! wider [`crate::AgentIdentity`] wrapper struct on
//! [`crate::RuntimeRequest`]. The shape is unchanged.

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

/// Free-form agent persona surfaced into the assembled system
/// prompt's `<agent_identity>` section.
///
/// Every field is best-effort prose: aura-os reads it off the
/// `agent_instance` row, the harness splices it into the prompt
/// verbatim. Empty / blank fields are dropped at render time so an
/// uninitialised wire payload produces no model-facing bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AgentPersona {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub personality: String,
}

impl AgentPersona {
    /// True when every field is blank — i.e. the wire payload
    /// carries no user-visible identity. The harness uses this to
    /// decide whether to populate `AgenticTaskParams::agent.identity`
    /// at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.role.trim().is_empty()
            && self.personality.trim().is_empty()
    }
}
