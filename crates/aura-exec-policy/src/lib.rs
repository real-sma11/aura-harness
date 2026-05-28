//! # aura-exec-policy
//!
//! Layer: exec
//!
//! Approval + verdict evaluation over already-resolved
//! [`EffectivePermissions`]. **This crate does no permission
//! intersection math** — that's the job of
//! [`aura_core_permissions::math::narrow`] /
//! [`aura_core_permissions::math::intersect`]. The exec policy gate
//! receives a fully resolved bundle (the result of
//! `mode ∩ user_grants ∩ parent_chain`) and decides whether a single
//! tool call's required capability set is satisfied.
//!
//! ## Invariants ([rules.md §13])
//!
//! - The check is **purely satisfaction-based**: each entry in
//!   [`ToolApproval::required`] must be satisfied by *some* grant in
//!   `perms.permissions.capabilities` (via
//!   [`Capability::satisfies`], which honours the wildcard lifting
//!   rules for project capabilities). The first unsatisfied
//!   capability short-circuits with
//!   [`PolicyError::CapabilityDenied`].
//! - The empty `required` slice always evaluates to `Ok(())` — tools
//!   with no declared capability requirement are universally visible,
//!   matching the pre-Phase-5 behaviour of `Tool::required_capabilities`
//!   returning an empty vec.
//! - This crate **never consults env vars, files, or any external
//!   side channel**. The verdict is a pure function of its inputs so
//!   the agent loop can replay the decision deterministically.
//!
//! ## Failure modes
//!
//! - [`PolicyError::CapabilityDenied`] — at least one required
//!   capability was not held. Carries the missing capability for
//!   logging / UI rendering.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use aura_core_permissions::{Capability, EffectivePermissions};
use thiserror::Error;

/// Errors returned by [`evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PolicyError {
    /// The caller does not hold a grant satisfying the named
    /// capability. The contained value is the first missing capability
    /// (subsequent unsatisfied capabilities short-circuit).
    #[error("capability denied: {0:?}")]
    CapabilityDenied(Capability),
}

/// Approval input for a single tool call.
///
/// Constructed by the runner at proposal time and consumed by
/// [`evaluate`]. The `tool_name` is carried purely for logging; the
/// gate's verdict depends only on `required`.
#[derive(Debug, Clone, Copy)]
pub struct ToolApproval<'a> {
    /// Tool name, surfaced in tracing / verdict logs.
    pub tool_name: &'a str,
    /// Capabilities the caller must hold for this tool to execute.
    pub required: &'a [Capability],
}

/// Decide whether `perms` satisfies `approval`.
///
/// # Errors
///
/// Returns [`PolicyError::CapabilityDenied`] for the first missing
/// capability (in `approval.required` order). The empty slice is
/// always allowed.
pub fn evaluate(
    perms: &EffectivePermissions,
    approval: ToolApproval<'_>,
) -> Result<(), PolicyError> {
    for cap in approval.required {
        if !perms
            .permissions
            .capabilities
            .iter()
            .any(|held| held.satisfies(cap))
        {
            return Err(PolicyError::CapabilityDenied(cap.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core_permissions::Permissions;

    fn perms_with(caps: Vec<Capability>) -> EffectivePermissions {
        EffectivePermissions {
            permissions: Permissions {
                scope: Default::default(),
                capabilities: caps,
            },
            grants: Vec::new(),
        }
    }

    #[test]
    fn empty_required_is_allowed() {
        let perms = perms_with(vec![]);
        let approval = ToolApproval {
            tool_name: "list_files",
            required: &[],
        };
        assert!(evaluate(&perms, approval).is_ok());
    }

    #[test]
    fn matching_capability_is_allowed() {
        let perms = perms_with(vec![Capability::SpawnAgent]);
        let approval = ToolApproval {
            tool_name: "spawn_agent",
            required: &[Capability::SpawnAgent],
        };
        assert!(evaluate(&perms, approval).is_ok());
    }

    #[test]
    fn missing_capability_is_denied() {
        let perms = perms_with(vec![]);
        let required = [Capability::SpawnAgent];
        let approval = ToolApproval {
            tool_name: "spawn_agent",
            required: &required,
        };
        let err = evaluate(&perms, approval).expect_err("missing cap must deny");
        assert_eq!(err, PolicyError::CapabilityDenied(Capability::SpawnAgent));
    }

    #[test]
    fn project_write_satisfies_project_read() {
        let perms = perms_with(vec![Capability::WriteProject { id: "p".into() }]);
        let required = [Capability::ReadProject { id: "p".into() }];
        let approval = ToolApproval {
            tool_name: "read_file",
            required: &required,
        };
        assert!(evaluate(&perms, approval).is_ok());
    }

    #[test]
    fn wildcard_write_satisfies_specific_write() {
        let perms = perms_with(vec![Capability::WriteAllProjects]);
        let required = [Capability::WriteProject {
            id: "anything".into(),
        }];
        let approval = ToolApproval {
            tool_name: "write_file",
            required: &required,
        };
        assert!(evaluate(&perms, approval).is_ok());
    }

    #[test]
    fn first_missing_capability_short_circuits() {
        let perms = perms_with(vec![Capability::SpawnAgent]);
        let required = [Capability::ListAgents, Capability::SpawnAgent];
        let approval = ToolApproval {
            tool_name: "list_then_spawn",
            required: &required,
        };
        let err = evaluate(&perms, approval).expect_err("missing first cap denies");
        assert_eq!(err, PolicyError::CapabilityDenied(Capability::ListAgents));
    }

    #[test]
    fn multiple_required_all_held_is_allowed() {
        let perms = perms_with(vec![
            Capability::SpawnAgent,
            Capability::ControlAgent,
            Capability::ReadAgent,
        ]);
        let required = [Capability::SpawnAgent, Capability::ControlAgent];
        let approval = ToolApproval {
            tool_name: "spawn_then_send",
            required: &required,
        };
        assert!(evaluate(&perms, approval).is_ok());
    }
}
