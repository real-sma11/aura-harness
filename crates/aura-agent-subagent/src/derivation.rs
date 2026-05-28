//! [`SubagentDerivation`] trait + [`DefaultDerivation`] impl.

use aura_core_modes::{AgentMode, JoinPolicy, KernelMode, ModeProfile, ReplayMode, SpawnMode};

use crate::errors::DerivationError;
use crate::manifest::{OverriddenField, OverrideManifest};
use crate::overrides::{SubagentBudget, SubagentOverrides};
use crate::parent::ParentContext;
use crate::spec::{AuditAttribution, SubagentLineage, SubagentSpec};

/// Configuration consumed by [`DefaultDerivation`]. Wired from
/// `aura-config` in Phase 7a; Phase 6a hard-codes a sensible default
/// through [`SubagentDerivationConfig::default_for_phase_6a`].
#[derive(Clone, Debug)]
pub struct SubagentDerivationConfig {
    /// Hard cap on subagent depth (depth 0 = root agent).
    pub max_depth: u32,
    /// Default [`KernelMode`] for derived children when no override
    /// is supplied. Per the architecture plan children default to
    /// [`KernelMode::AuditedLite`].
    pub default_kernel_mode_for_subagents: KernelMode,
    /// Default budget seeded into derived children.
    pub default_budget: SubagentBudget,
}

impl SubagentDerivationConfig {
    /// Phase 6a defaults: depth cap 8, AuditedLite per child,
    /// 64K-token / 50-iter / 5-min budget.
    #[must_use]
    pub fn default_for_phase_6a() -> Self {
        Self {
            max_depth: 8,
            default_kernel_mode_for_subagents: KernelMode::AuditedLite,
            default_budget: SubagentBudget::default_for_phase_6a(),
        }
    }
}

impl Default for SubagentDerivationConfig {
    fn default() -> Self {
        Self::default_for_phase_6a()
    }
}

/// Trait every derivation impl satisfies. Trait surface kept narrow
/// so test doubles can swap in a stub derivation without dragging
/// the entire config surface.
pub trait SubagentDerivation: Send + Sync {
    /// Derive a [`SubagentSpec`] + [`OverrideManifest`] from the
    /// captured [`ParentContext`] and caller-supplied
    /// [`SubagentOverrides`].
    ///
    /// # Errors
    ///
    /// Returns [`DerivationError`] for every taxonomy violation
    /// documented at the crate root.
    fn derive(
        &self,
        parent: &ParentContext,
        overrides: SubagentOverrides,
    ) -> Result<(SubagentSpec, OverrideManifest), DerivationError>;
}

/// Closed-enum derivation strategy used in production.
#[derive(Clone, Debug)]
pub struct DefaultDerivation {
    /// Configuration applied to every derivation.
    pub config: SubagentDerivationConfig,
}

impl DefaultDerivation {
    /// Construct a [`DefaultDerivation`] with the given config.
    #[must_use]
    pub fn new(config: SubagentDerivationConfig) -> Self {
        Self { config }
    }
}

impl Default for DefaultDerivation {
    fn default() -> Self {
        Self::new(SubagentDerivationConfig::default())
    }
}

impl SubagentDerivation for DefaultDerivation {
    fn derive(
        &self,
        parent: &ParentContext,
        overrides: SubagentOverrides,
    ) -> Result<(SubagentSpec, OverrideManifest), DerivationError> {
        // 1. Depth cap — checked first so deep recursion never holds
        //    fleet-layer resources.
        let new_depth = parent.depth.saturating_add(1);
        if new_depth > self.config.max_depth {
            return Err(DerivationError::DepthExceeded {
                parent_depth: parent.depth,
                max_depth: self.config.max_depth,
            });
        }

        let mut manifest = OverrideManifest::default();

        // 2. Mode override (narrowing-only). Checked BEFORE
        //    `allows_spawn` so a malformed widening request gets a
        //    precise `ModeWidens` rejection rather than the broader
        //    `SpawnNotAllowed` mask when the parent's own mode also
        //    fails the spawn-allowed gate.
        let mode = if let Some(requested) = overrides.mode {
            if !narrowing_allowed(parent.mode, requested) {
                return Err(DerivationError::ModeWidens {
                    parent: parent.mode,
                    requested,
                });
            }
            if requested != parent.mode {
                manifest.applied.push(OverriddenField::Mode {
                    from: parent.mode,
                    to: requested,
                });
            }
            requested
        } else {
            parent.mode
        };

        // 3. Spawn-allowed mode gate. Per the mode table only
        //    `AgentMode::Agent` currently permits spawning; the rule
        //    lives in `aura_core_modes::AgentMode::allows_spawn` so
        //    the table is the single source of truth.
        if !parent.mode.allows_spawn() {
            return Err(DerivationError::SpawnNotAllowed(parent.mode));
        }

        // 4. Permissions override (intersection-only — any
        //    capability present in the request but not in the parent
        //    is a widen).
        let permissions = if let Some(requested) = overrides.permissions {
            if let Some(cap) = first_widening_capability(&parent.permissions, &requested) {
                return Err(DerivationError::PermissionsWiden {
                    capability: cap.clone(),
                });
            }
            let narrowed = aura_core_permissions::narrow(&parent.permissions, &requested);
            manifest.applied.push(OverriddenField::Permissions {
                capability_count: narrowed.capabilities.len(),
            });
            narrowed
        } else {
            parent.permissions.clone()
        };

        // 5. Kernel-mode override — only legal direction is to
        //    upgrade audit (AuditedLite → Audited). Downgrading is
        //    forbidden so the kernel always sees at least
        //    AuditedLite per the rules.md §13 invariant.
        let kernel_mode_default = self.config.default_kernel_mode_for_subagents;
        let parent_kernel = parent.kernel_mode();
        let kernel_mode = if let Some(requested) = overrides.kernel_mode {
            if downgrades_kernel(parent_kernel, requested) {
                return Err(DerivationError::KernelModeDowngradeForbidden {
                    parent: parent_kernel,
                    requested,
                });
            }
            if requested != kernel_mode_default {
                manifest.applied.push(OverriddenField::KernelMode {
                    from: kernel_mode_default,
                    to: requested,
                });
            }
            requested
        } else {
            kernel_mode_default
        };

        // 6. Model id (free-form string in Phase 6a; richer
        //    permission gating lands in Phase 7+).
        let model_id = if let Some(requested) = overrides.model_id {
            if requested != parent.model_id {
                manifest.applied.push(OverriddenField::ModelId {
                    from: parent.model_id.clone(),
                    to: requested.clone(),
                });
            }
            requested
        } else {
            parent.model_id.clone()
        };

        // 7. Remaining purely-additive overrides.
        let kind = if let Some(kind) = overrides.kind {
            manifest.applied.push(OverriddenField::Kind(kind.clone()));
            kind
        } else {
            "task".to_string()
        };

        let spawn_mode = if let Some(sm) = overrides.spawn_mode {
            manifest.applied.push(OverriddenField::SpawnMode(sm));
            sm
        } else {
            SpawnMode::Wait
        };

        let join_policy = if let Some(jp) = overrides.join_policy {
            manifest.applied.push(OverriddenField::JoinPolicy(jp));
            jp
        } else {
            JoinPolicy::All
        };

        let replay_mode = if let Some(rm) = overrides.replay_mode {
            manifest.applied.push(OverriddenField::ReplayMode(rm));
            rm
        } else {
            ReplayMode::Live
        };

        let budget = if let Some(b) = overrides.budget {
            manifest.applied.push(OverriddenField::Budget);
            b
        } else {
            self.config.default_budget.clone()
        };

        if let Some(subset) = &overrides.tool_subset {
            manifest.applied.push(OverriddenField::ToolSubset {
                count: subset.len(),
            });
        }
        if let Some(iso) = &overrides.isolation_id {
            manifest
                .applied
                .push(OverriddenField::IsolationId(iso.clone()));
        }
        if let Some(ty) = &overrides.subagent_type {
            manifest
                .applied
                .push(OverriddenField::SubagentType(ty.clone()));
        }
        if let Some(addendum) = &overrides.system_prompt_addendum {
            manifest
                .applied
                .push(OverriddenField::SystemPromptAddendum {
                    chars: addendum.chars().count(),
                });
        }
        if let Some(perms) = &overrides.parent_tool_permissions {
            manifest
                .applied
                .push(OverriddenField::ParentToolPermissions {
                    entries: perms.per_tool.len(),
                });
        }
        if overrides.user_tool_defaults.is_some() {
            manifest.applied.push(OverriddenField::UserToolDefaults);
        }

        // 8. Compose the resolved ModeProfile from the parent's
        //    profile, swapping in the (possibly overridden) kernel
        //    mode and the (possibly overridden) AgentMode.
        let mode_profile = ModeProfile {
            agent: mode,
            kernel: kernel_mode,
            sandbox: parent.mode_profile.sandbox,
            replay: parent.mode_profile.replay,
        };

        // 9. Extend the lineage chain to include the parent agent.
        let mut chain = parent.lineage.chain.clone();
        if chain.last().copied() != Some(parent.agent_id) {
            chain.push(parent.agent_id);
        }
        let lineage = SubagentLineage {
            root_agent_id: parent.lineage.root_agent_id,
            chain,
        };

        let spec = SubagentSpec {
            parent: parent.agent_id,
            depth: new_depth,
            mode,
            mode_profile,
            permissions,
            kernel_mode,
            model_id,
            kind,
            spawn_mode,
            join_policy,
            replay_mode,
            budget,
            tool_subset: overrides.tool_subset,
            isolation_id: overrides.isolation_id,
            lineage,
            audit_attribution: AuditAttribution {
                parent_agent_id: parent.agent_id,
            },
            overridden_fields: manifest.clone(),
            subagent_type: overrides.subagent_type,
            system_prompt_addendum: overrides.system_prompt_addendum,
            parent_tool_permissions: overrides.parent_tool_permissions,
            user_tool_defaults: overrides.user_tool_defaults,
        };

        Ok((spec, manifest))
    }
}

/// Mode-narrowing table. Returns `true` iff `child` is a legal
/// narrowing of `parent` under the rules.md §13 mode rule.
///
/// | parent → child | Agent | Plan | Ask | Debug |
/// |----------------|-------|------|-----|-------|
/// | Agent          | ✅    | ✅   | ✅  | ✅    |
/// | Plan           | ❌    | ✅   | ✅  | ✅    |
/// | Ask            | ❌    | ❌   | ✅  | ❌    |
/// | Debug          | ❌    | ❌   | ❌  | ✅    |
#[must_use]
fn narrowing_allowed(parent: AgentMode, child: AgentMode) -> bool {
    use AgentMode::{Agent, Ask, Debug, Plan};
    matches!(
        (parent, child),
        (Agent, _) | (Plan, Plan | Ask | Debug) | (Ask, Ask) | (Debug, Debug)
    )
}

/// Returns the first capability in `requested` that is not held by
/// `parent`. Used to fail-fast on widening attempts.
fn first_widening_capability<'a>(
    parent: &aura_core_permissions::Permissions,
    requested: &'a aura_core_permissions::Permissions,
) -> Option<&'a aura_core_permissions::Capability> {
    requested.capabilities.iter().find(|requested_cap| {
        !parent
            .capabilities
            .iter()
            .any(|held| held.satisfies(requested_cap))
    })
}

/// True iff `requested` is strictly less audited than `parent`.
///
/// Audit ordering: `AuditedLite < Audited`. Downgrading `Audited →
/// AuditedLite` IS forbidden by `KernelModeDowngradeForbidden`;
/// every other transition is fine (same-mode, or AuditedLite →
/// Audited upgrade).
#[must_use]
fn downgrades_kernel(parent: KernelMode, requested: KernelMode) -> bool {
    matches!(
        (parent, requested),
        (KernelMode::Audited, KernelMode::AuditedLite)
    )
}

/// Placeholder for the flow-derived spawn entrypoint promised in
/// Section 4 of the architecture plan. Phase 7+ wires it; today it is
/// a marker trait so call sites can target the future shape.
pub trait FlowDerivation: Send + Sync {}
