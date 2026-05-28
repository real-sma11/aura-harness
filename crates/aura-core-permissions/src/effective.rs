//! [`EffectivePermissions`] — the `mode ∩ user_grants` view.

use serde::{Deserialize, Serialize};

use aura_core_modes::AgentMode;

use crate::capability::Capability;
use crate::grants::PrivilegeGrant;
use crate::Permissions;

/// Mode-narrowed permissions view.
///
/// Computed by [`effective`] as
/// `effective = AgentMode::default_capability_profile() ∩ user_grants`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectivePermissions {
    /// The narrowed permission set actually in force.
    pub permissions: Permissions,
    /// Provenance of every retained capability (debug/audit only).
    #[serde(default)]
    pub grants: Vec<PrivilegeGrant>,
}

/// Verdict returned by the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    /// The action is permitted.
    Allow,
    /// The action is denied.
    Deny(PermissionError),
}

/// Reasons a permission check can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermissionError {
    /// No grant in the effective set satisfies the required
    /// capability.
    #[error("required capability not granted: {0}")]
    MissingCapability(&'static str),
    /// The scope does not include the requested target.
    #[error("scope does not include {axis}={value}")]
    OutOfScope {
        /// `org`, `project`, or `agent_id`.
        axis: &'static str,
        /// The identifier that was out of scope.
        value: String,
    },
}

/// Compute `effective = mode-default ∩ user_grants`.
///
/// Returns a bundle whose capability list is the subset of
/// `user_grants.capabilities` whose discriminants are also allowed by
/// `mode.default_capability_profile()`. Scope is preserved verbatim
/// from `user_grants` (mode does not narrow scope today).
#[must_use]
pub fn effective(mode: AgentMode, user_grants: &Permissions) -> EffectivePermissions {
    let profile = mode.default_capability_profile();
    let capabilities: Vec<Capability> = user_grants
        .capabilities
        .iter()
        .filter(|cap| profile.contains(cap.discriminant()))
        .cloned()
        .collect();
    EffectivePermissions {
        permissions: Permissions {
            scope: user_grants.scope.clone(),
            capabilities,
        },
        grants: Vec::new(),
    }
}
