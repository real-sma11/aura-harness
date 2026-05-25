//! Wire-compatible agent identity bundle.
//!
//! Mirrors `aura_agent::prompts::AgentIdentity`'s shape on the
//! `AutomatonStartRequest` path. The harness consumes this in the
//! automaton bridge and re-borrows it as
//! `aura_agent::prompts::AgentIdentity<'_>` when assembling
//! [`AgenticTaskParams::agent`].
//!
//! Lives in `aura-protocol` so the aura-os producer side and the
//! harness consumer side share a single source of truth without
//! either repo depending on the other's domain crate.
//!
//! [`AgenticTaskParams::agent`]: # "documented in aura-agent"

use serde::{Deserialize, Serialize};

#[cfg(feature = "typescript")]
use ts_rs::TS;

/// Free-form agent identity surfaced into the dev-loop / task-run
/// system prompt's `<agent_identity>` section.
///
/// Every field is best-effort prose: aura-os reads it off the
/// `agent_instance` row, the harness splices it into the prompt
/// verbatim. Empty / blank fields are dropped at render time so an
/// uninitialised wire payload (PR B's default) produces no model-facing
/// bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "typescript", derive(TS), ts(export))]
pub struct AgentIdentityWire {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub personality: String,
}

impl AgentIdentityWire {
    /// True when every field is blank — i.e. the wire payload carries
    /// no user-visible identity. The harness uses this to decide
    /// whether to populate `AgenticTaskParams::agent.identity` at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.role.trim().is_empty()
            && self.personality.trim().is_empty()
    }
}
