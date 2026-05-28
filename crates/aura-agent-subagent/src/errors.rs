//! [`DerivationError`] — closed taxonomy of subagent derivation
//! failures.

use aura_core_modes::{AgentMode, KernelMode};
use aura_core_permissions::Capability;
use thiserror::Error;

/// Reasons a [`crate::SubagentDerivation::derive`] call can refuse to
/// produce a [`crate::SubagentSpec`].
///
/// Every variant is covered by a dedicated unit test in
/// [`crate::tests`] so the closed-enum invariant is enforced.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DerivationError {
    /// Subagent depth would exceed the configured maximum. Computed
    /// before any other validation so deep recursion never holds
    /// fleet-layer resources.
    #[error("subagent depth exceeded: parent depth {parent_depth}, max {max_depth}")]
    DepthExceeded {
        /// Parent agent's current depth.
        parent_depth: u32,
        /// Configured maximum subagent depth.
        max_depth: u32,
    },

    /// The requested override would widen the parent's
    /// [`aura_core_permissions::Permissions`]. The reported
    /// `capability` is the first cap in the request that the parent
    /// does not hold.
    #[error(
        "derived permissions widen parent's: requested capability {capability:?} not in parent bundle"
    )]
    PermissionsWiden {
        /// First widening capability found in the requested override.
        capability: Capability,
    },

    /// The requested child mode is not reachable from the parent mode
    /// under the mode-narrowing table.
    #[error("mode widens parent's: parent={parent:?}, requested={requested:?}")]
    ModeWidens {
        /// Parent's current [`AgentMode`].
        parent: AgentMode,
        /// Child mode the override requested.
        requested: AgentMode,
    },

    /// The parent's [`AgentMode`] does not permit any subagent spawn.
    /// Today this is every mode except [`AgentMode::Agent`].
    #[error("spawn not allowed in parent mode {0:?}")]
    SpawnNotAllowed(AgentMode),

    /// The requested [`KernelMode`] would downgrade audit. The kernel
    /// is the sole writer of [`aura_store_record`] records and may
    /// never be bypassed; [`KernelMode::AuditedLite`] is the lightest
    /// supported tier.
    #[error("kernel-mode downgrade forbidden: requested={requested:?}, parent={parent:?}")]
    KernelModeDowngradeForbidden {
        /// Parent's current [`KernelMode`].
        parent: KernelMode,
        /// Override the caller requested.
        requested: KernelMode,
    },

    /// Override map carried a field that may not be overridden (e.g.
    /// `UserId`, audit attribution), or was structurally invalid.
    #[error("invalid override: {0}")]
    InvalidOverride(String),
}
