//! [`ParentContext`] — atomic snapshot of a parent agent's session
//! state at spawn time.

use aura_core::AgentId;
use aura_core_modes::{AgentMode, KernelMode, ModeProfile};
use aura_core_permissions::Permissions;

use crate::spec::SubagentLineage;

/// Stable snapshot of the parent agent's spawn-time session state.
///
/// Captured atomically by the spawning tool before
/// [`crate::SubagentDerivation::derive`] runs so concurrent parent
/// state changes can never race the derivation rules.
///
/// Phase 6a intentionally carries a minimum-viable field set —
/// `(mode, mode_profile, permissions, model_id, depth, lineage)` —
/// rather than the full Section-4 sketch (plugin sets, MCP, hook,
/// env, isolation, etc.). The wider snapshot lands in Phase 7+ once
/// the plugin / context surfaces stabilise; the closed-enum
/// invariants and narrowing math already live here so the API
/// extension is additive.
#[derive(Clone, Debug)]
pub struct ParentContext {
    /// Parent agent's stable identifier.
    pub agent_id: AgentId,
    /// Parent depth from the root agent (root = 0).
    pub depth: u32,
    /// Parent's resolved [`AgentMode`].
    pub mode: AgentMode,
    /// Parent's resolved [`ModeProfile`] (kernel / sandbox / replay
    /// bundle).
    pub mode_profile: ModeProfile,
    /// Parent's effective [`Permissions`] (already intersected with
    /// the parent's mode default).
    pub permissions: Permissions,
    /// Parent's selected model identifier (free-form string today;
    /// `ModelId` newtype landing in `aura-model-reasoner`).
    pub model_id: String,
    /// Parent → root agent lineage. Defaults to a chain consisting
    /// of just the parent agent itself.
    pub lineage: SubagentLineage,
}

impl ParentContext {
    /// Convenience getter for the parent's audit [`KernelMode`]
    /// surfaced from the parent [`ModeProfile`].
    #[must_use]
    pub fn kernel_mode(&self) -> KernelMode {
        self.mode_profile.kernel
    }
}
