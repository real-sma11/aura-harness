//! [`OverrideManifest`] / [`OverriddenField`] — typed record of which
//! fields were explicitly overridden vs inherited.
//!
//! Written into `RecordKind::SubagentSpawn` payloads (Phase 7a) so an
//! auditor can replay parent intent without diffing the resolved
//! [`crate::SubagentSpec`] against the parent context.

use aura_core_modes::{AgentMode, JoinPolicy, KernelMode, ReplayMode, SpawnMode};
use serde::{Deserialize, Serialize};

/// Single explicit override entry.
///
/// `Serialize` + `Deserialize` derives land in Phase 7a so the
/// fleet-layer spawn writes the manifest into the audit log
/// (`RecordKind::SubagentSpawn` payload) via
/// `aura-agent-kernel::write_system_record`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
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
    /// Phase 7b: bundled subagent type was selected explicitly.
    SubagentType(String),
    /// Phase 7b: system prompt addendum was supplied.
    SystemPromptAddendum {
        /// Character length of the addendum (the addendum body
        /// itself is recorded inside `SubagentSpec`, not duplicated
        /// here).
        chars: usize,
    },
    /// Phase 7b: parent's per-tool override map was forwarded
    /// explicitly with this count of per-tool entries.
    ParentToolPermissions {
        /// Number of per-tool entries in the parent override map.
        entries: usize,
    },
    /// Phase 7b: explicit user-tool defaults override.
    UserToolDefaults,
}

/// Manifest of fields a caller explicitly overrode during
/// derivation.
///
/// `Serialize` + `Deserialize` derives land in Phase 7a so the
/// fleet-layer spawn writes the manifest into the audit log via
/// `aura-agent-kernel::write_system_record`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
