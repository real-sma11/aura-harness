//! [`OverrideManifest`] / [`OverriddenField`] — typed record of which
//! fields were explicitly overridden vs inherited.
//!
//! Written into `RecordKind::SubagentSpawn` payloads (Phase 7a) so an
//! auditor can replay parent intent without diffing the resolved
//! [`crate::SubagentSpec`] against the parent context.

use aura_core_modes::{AgentMode, JoinPolicy, KernelMode, ReplayMode, SpawnMode};

/// Single explicit override entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverriddenField {
    /// Mode was narrowed from `from` to `to`.
    Mode {
        /// Parent mode.
        from: AgentMode,
        /// Child mode after override.
        to: AgentMode,
    },
    /// Permissions were narrowed; the count of capabilities
    /// preserved in the child.
    Permissions {
        /// Number of capabilities preserved after intersection.
        capability_count: usize,
    },
    /// Kernel mode was upgraded (only direction allowed).
    KernelMode {
        /// Parent kernel mode.
        from: KernelMode,
        /// Child kernel mode after override (strictly more audited
        /// than `from`).
        to: KernelMode,
    },
    /// Model id was overridden.
    ModelId {
        /// Parent model id.
        from: String,
        /// Override model id.
        to: String,
    },
    /// Kind tag was set explicitly.
    Kind(String),
    /// Spawn mode was set explicitly.
    SpawnMode(SpawnMode),
    /// Join policy was set explicitly.
    JoinPolicy(JoinPolicy),
    /// Replay mode was overridden (rare; only legal target today is
    /// the same as the default).
    ReplayMode(ReplayMode),
    /// Budget was set explicitly.
    Budget,
    /// Tool subset was set explicitly with this count of tools.
    ToolSubset {
        /// Number of tools the child is restricted to.
        count: usize,
    },
    /// Isolation environment id was set explicitly.
    IsolationId(String),
}

/// Manifest of fields a caller explicitly overrode during
/// derivation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OverrideManifest {
    /// Applied overrides in declaration order.
    pub applied: Vec<OverriddenField>,
}

impl OverrideManifest {
    /// True iff no explicit overrides were applied — the child
    /// inherits every field from the parent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}
