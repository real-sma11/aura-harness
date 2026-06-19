//! Pure-data adapters between [`aura_core_types`] dispatch primitives and
//! the [`crate`] (agent-layer) derivation surface.
//!
//! Phase B / Commit 3 / Step 3a moved these helpers out of the
//! gateway-side `aura-runtime/src/subagent_dispatch.rs` so the
//! fleet-layer dispatcher can be sliced cleanly out. None of them
//! touch fleet types — they translate parent context, narrowing
//! policy, and override fields between the wire/core types in
//! `aura_core_types` and the agent-layer types this crate already owns.

use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode};
use aura_core_permissions::Permissions;
use aura_core_types::{AgentPermissions, SubagentDispatchRequest, SubagentKindSpec};

use crate::{ParentContext, SubagentLineage, SubagentOverrides};

/// Build the [`ParentContext`] consumed by
/// [`crate::DefaultDerivation`].
///
/// Phase 7b honours every optional snapshot field on the request
/// (`parent_mode`, `parent_kernel_mode`, `parent_model_id`); when a
/// field is `None` the legacy Phase 7a defaults apply so old callers
/// continue to work.
#[must_use]
pub fn parent_context_from_request(request: &SubagentDispatchRequest) -> ParentContext {
    let lineage = if request.parent_chain.is_empty() {
        SubagentLineage::from_root(request.parent_agent_id)
    } else {
        SubagentLineage {
            root_agent_id: request
                .parent_chain
                .last()
                .copied()
                .unwrap_or(request.parent_agent_id),
            chain: request.parent_chain.clone(),
        }
    };
    let permissions = legacy_permissions_to_modes(&request.parent_permissions);
    let depth = u32::try_from(request.parent_chain.len()).unwrap_or(u32::MAX);
    let mode = request
        .parent_mode
        .map_or(AgentMode::Agent, core_to_modes_mode);
    let kernel = request
        .parent_kernel_mode
        .map_or(KernelMode::Audited, core_to_modes_kernel);
    let model_id = request.parent_model_id.clone().unwrap_or_default();
    ParentContext {
        agent_id: request.parent_agent_id,
        depth,
        mode,
        mode_profile: ModeProfile {
            agent: mode,
            kernel,
            sandbox: SandboxMode::Standard,
            replay: ReplayMode::Live,
        },
        permissions,
        model_id,
        lineage,
    }
}

/// Translate the legacy [`aura_core_types::AgentMode`] enum into the
/// canonical [`aura_core_modes::AgentMode`]. The two enums are
/// structurally identical today (one re-exports the other); the
/// alias keeps call sites at a single readable level of indirection.
#[must_use]
pub fn core_to_modes_mode(mode: aura_core_types::AgentMode) -> AgentMode {
    match mode {
        aura_core_types::AgentMode::Agent => AgentMode::Agent,
        aura_core_types::AgentMode::Plan => AgentMode::Plan,
        aura_core_types::AgentMode::Ask => AgentMode::Ask,
        aura_core_types::AgentMode::Debug => AgentMode::Debug,
    }
}

/// Translate [`aura_core_types::KernelMode`] into [`aura_core_modes::KernelMode`].
#[must_use]
pub fn core_to_modes_kernel(kernel: aura_core_types::KernelMode) -> KernelMode {
    match kernel {
        aura_core_types::KernelMode::Audited => KernelMode::Audited,
        aura_core_types::KernelMode::AuditedLite => KernelMode::AuditedLite,
    }
}

/// Build [`SubagentOverrides`] from the parent's request +
/// the resolved kind. Phase 7b absorbs the previously-out-of-band
/// fields directly into the overrides struct so the spawner no
/// longer needs a separate compat carrier.
#[must_use]
pub fn overrides_from_request(
    request: &SubagentDispatchRequest,
    kind: &SubagentKindSpec,
) -> SubagentOverrides {
    let narrowed_parent = narrow_permissions(&request.parent_permissions, kind);
    let mode_override = request.override_mode.map(core_to_modes_mode);
    let tool_subset = request
        .override_tool_subset
        .clone()
        .unwrap_or_else(|| kind.allowed_tools.clone());
    let permissions = if let Some(explicit) = &request.override_permissions {
        // Honour the explicit override (subject to derivation's
        // narrowing-only rule); intersect with kind defaults so the
        // child still cannot widen past the kind's allow-list.
        let restricted = narrow_permissions(explicit, kind);
        Some(legacy_permissions_to_modes(&restricted))
    } else {
        Some(legacy_permissions_to_modes(&narrowed_parent))
    };
    let budget = request
        .override_budget
        .clone()
        .map(|b| crate::SubagentBudget {
            max_tokens: b.max_tokens.unwrap_or(64_000),
            max_iterations: b.max_iterations,
            timeout_ms: b.timeout_ms,
        });
    SubagentOverrides {
        mode: mode_override,
        permissions,
        kernel_mode: None,
        model_id: request
            .model_override
            .clone()
            .or_else(|| kind.default_model.clone()),
        kind: Some(kind.name.clone()),
        spawn_mode: request.spawn_mode,
        join_policy: None,
        replay_mode: None,
        budget,
        tool_subset: Some(tool_subset),
        isolation_id: request.override_isolation_id.clone(),
        subagent_type: Some(kind.name.clone()),
        system_prompt_addendum: request.system_prompt_addendum.clone(),
        parent_tool_permissions: request.parent_tool_permissions.clone(),
        user_tool_defaults: Some(request.user_tool_defaults.clone()),
    }
}

/// Intersect the parent's permissions with the kind's allow-list so a
/// derived child can never widen the parent's capability set or step
/// outside the kind's declared surface.
#[must_use]
pub fn narrow_permissions(parent: &AgentPermissions, kind: &SubagentKindSpec) -> AgentPermissions {
    let capabilities = parent
        .capabilities
        .iter()
        .filter(|held| {
            kind.allowed_capabilities
                .iter()
                .any(|allowed| held.satisfies(allowed))
        })
        .cloned()
        .collect();
    AgentPermissions {
        scope: parent.scope.clone(),
        capabilities,
    }
}

/// Map the legacy [`aura_core_types::AgentPermissions`] surface onto the
/// [`aura_core_permissions::Permissions`] type the derivation engine
/// consumes. The two shapes carry the same data; the alias clarifies
/// the layer hop.
#[must_use]
pub fn legacy_permissions_to_modes(legacy: &AgentPermissions) -> Permissions {
    Permissions {
        scope: legacy.scope.clone(),
        capabilities: legacy.capabilities.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SubagentRegistry;
    use aura_core_types::{AgentScope, Capability};

    #[test]
    fn explore_subagent_retains_parent_invoke_process_for_verification() {
        let registry = SubagentRegistry::bundled();
        let kind = registry.get("explore").unwrap();
        let parent = AgentPermissions {
            scope: AgentScope::default(),
            capabilities: vec![
                Capability::SpawnAgent,
                Capability::ReadAgent,
                Capability::InvokeProcess,
            ],
        };

        let narrowed = narrow_permissions(&parent, kind);

        assert_eq!(narrowed.scope, parent.scope);
        assert!(
            narrowed
                .capabilities
                .iter()
                .any(|capability| *capability == Capability::InvokeProcess),
            "explore subagents need InvokeProcess to run verification commands"
        );
        assert!(
            !narrowed
                .capabilities
                .iter()
                .any(|capability| *capability == Capability::SpawnAgent),
            "subagents must not inherit spawn unless the kind explicitly allows it"
        );
    }
}
