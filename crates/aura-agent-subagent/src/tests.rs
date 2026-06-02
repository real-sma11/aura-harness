//! Unit tests covering every [`DerivationError`] variant + the
//! narrowing-only invariant proptest.

use aura_core_modes::{AgentMode, KernelMode, ModeProfile, SandboxMode};
use aura_core_permissions::{Capability, Permissions};
use aura_core_types::AgentId;
use proptest::prelude::*;

use crate::{
    DefaultDerivation, DerivationError, ParentContext, SubagentBudget, SubagentDerivation,
    SubagentDerivationConfig, SubagentLineage, SubagentOverrides,
};

fn parent(mode: AgentMode, kernel: KernelMode) -> ParentContext {
    let agent_id = AgentId::generate();
    let profile = ModeProfile {
        agent: mode,
        kernel,
        sandbox: SandboxMode::Standard,
        replay: aura_core_modes::ReplayMode::Live,
    };
    ParentContext {
        agent_id,
        depth: 0,
        mode,
        mode_profile: profile,
        permissions: Permissions {
            scope: aura_core_permissions::AgentScope::default(),
            capabilities: vec![Capability::ReadAllProjects, Capability::SpawnAgent],
        },
        model_id: "claude-opus-4".to_string(),
        lineage: SubagentLineage::from_root(agent_id),
    }
}

fn config_with(max_depth: u32) -> SubagentDerivationConfig {
    let mut cfg = SubagentDerivationConfig::default_for_phase_6a();
    cfg.max_depth = max_depth;
    cfg
}

#[test]
fn derivation_error_depth_exceeded_fires_first() {
    let cfg = config_with(2);
    let derivation = DefaultDerivation::new(cfg);
    let mut p = parent(AgentMode::Agent, KernelMode::Audited);
    p.depth = 2;
    let err = derivation
        .derive(&p, SubagentOverrides::default())
        .expect_err("depth cap must trip");
    match err {
        DerivationError::DepthExceeded {
            parent_depth,
            max_depth,
        } => {
            assert_eq!(parent_depth, 2);
            assert_eq!(max_depth, 2);
        }
        other => panic!("expected DepthExceeded, got {other:?}"),
    }
}

#[test]
fn derivation_error_permissions_widen() {
    let derivation = DefaultDerivation::default();
    let p = parent(AgentMode::Agent, KernelMode::Audited);
    let widening = Permissions {
        capabilities: vec![Capability::WriteAllProjects],
        ..Permissions::default()
    };
    let err = derivation
        .derive(
            &p,
            SubagentOverrides {
                permissions: Some(widening),
                ..SubagentOverrides::default()
            },
        )
        .expect_err("widening permissions must trip");
    match err {
        DerivationError::PermissionsWiden { capability } => {
            assert_eq!(capability, Capability::WriteAllProjects);
        }
        other => panic!("expected PermissionsWiden, got {other:?}"),
    }
}

#[test]
fn derivation_error_mode_widens() {
    let derivation = DefaultDerivation::default();
    // Phase 7b moved the mode-widen check ahead of the
    // spawn-allowed gate so that a malformed widening override gets
    // a precise `ModeWidens` rejection rather than the broader
    // `SpawnNotAllowed` mask when the parent's own mode also fails
    // the spawn-allowed gate. A Plan parent requesting an Agent
    // child must therefore surface `ModeWidens`.
    let p = parent(AgentMode::Plan, KernelMode::Audited);
    let err = derivation
        .derive(
            &p,
            SubagentOverrides {
                mode: Some(AgentMode::Agent),
                ..SubagentOverrides::default()
            },
        )
        .expect_err("plan parent rejects agent override (widens)");
    let rendered = format!("{err}");
    assert!(rendered.contains("widens parent"), "{rendered}");
    match err {
        DerivationError::ModeWidens { parent, requested } => {
            assert_eq!(parent, AgentMode::Plan);
            assert_eq!(requested, AgentMode::Agent);
        }
        other => panic!("expected ModeWidens, got {other:?}"),
    }
}

#[test]
fn derivation_error_spawn_not_allowed_for_non_agent_modes() {
    let derivation = DefaultDerivation::default();
    for parent_mode in [AgentMode::Plan, AgentMode::Ask, AgentMode::Debug] {
        let p = parent(parent_mode, KernelMode::Audited);
        let err = derivation
            .derive(&p, SubagentOverrides::default())
            .expect_err("non-Agent parent cannot spawn");
        match err {
            DerivationError::SpawnNotAllowed(mode) => assert_eq!(mode, parent_mode),
            other => panic!("expected SpawnNotAllowed, got {other:?}"),
        }
    }
}

#[test]
fn derivation_error_kernel_mode_downgrade_forbidden() {
    let derivation = DefaultDerivation::default();
    let p = parent(AgentMode::Agent, KernelMode::Audited);
    let err = derivation
        .derive(
            &p,
            SubagentOverrides {
                kernel_mode: Some(KernelMode::AuditedLite),
                ..SubagentOverrides::default()
            },
        )
        .expect_err("Audited parent rejects AuditedLite child override");
    match err {
        DerivationError::KernelModeDowngradeForbidden { parent, requested } => {
            assert_eq!(parent, KernelMode::Audited);
            assert_eq!(requested, KernelMode::AuditedLite);
        }
        other => panic!("expected KernelModeDowngradeForbidden, got {other:?}"),
    }
}

#[test]
fn derivation_error_invalid_override_taxonomy_exercised() {
    // Phase 6a does not surface `InvalidOverride` through the
    // public API yet — the variant is reserved for non-overridable
    // fields like UserId / audit attribution wired in Phase 7+. We
    // still assert the variant constructs and renders correctly so
    // the closed taxonomy stays exercised end-to-end.
    let err = DerivationError::InvalidOverride("user_id cannot be overridden".into());
    let rendered = format!("{err}");
    assert!(rendered.contains("invalid override"), "{rendered}");
    assert!(rendered.contains("user_id"), "{rendered}");
}

#[test]
fn happy_path_inherit_everything_yields_empty_manifest() {
    let derivation = DefaultDerivation::default();
    let p = parent(AgentMode::Agent, KernelMode::Audited);
    let (spec, manifest) = derivation
        .derive(&p, SubagentOverrides::default())
        .expect("inheritance-only derivation must succeed");
    assert_eq!(spec.depth, 1);
    assert_eq!(spec.mode, AgentMode::Agent);
    // Children default to AuditedLite per the architecture plan.
    assert_eq!(spec.kernel_mode, KernelMode::AuditedLite);
    assert_eq!(spec.parent, p.agent_id);
    assert_eq!(spec.audit_attribution.parent_agent_id, p.agent_id);
    assert!(
        manifest.is_empty(),
        "no overrides applied → empty manifest: {:?}",
        manifest
    );
}

#[test]
fn overrides_are_recorded_in_manifest() {
    let derivation = DefaultDerivation::default();
    let p = parent(AgentMode::Agent, KernelMode::Audited);
    let (spec, manifest) = derivation
        .derive(
            &p,
            SubagentOverrides {
                mode: Some(AgentMode::Plan),
                kind: Some("reviewer".into()),
                model_id: Some("claude-haiku".into()),
                tool_subset: Some(vec!["read_file".into(), "list_files".into()]),
                ..SubagentOverrides::default()
            },
        )
        .expect("derivation");
    assert_eq!(spec.mode, AgentMode::Plan);
    assert_eq!(spec.kind, "reviewer");
    assert_eq!(spec.model_id, "claude-haiku");
    assert!(
        !manifest.is_empty(),
        "explicit overrides must populate the manifest"
    );
    assert!(manifest
        .applied
        .iter()
        .any(|f| matches!(f, crate::manifest::OverriddenField::Mode { .. })));
    assert!(manifest.applied.iter().any(|f| matches!(
        f,
        crate::manifest::OverriddenField::Kind(s) if s == "reviewer"
    )));
}

#[test]
fn default_budget_applied_when_omitted() {
    let derivation = DefaultDerivation::default();
    let p = parent(AgentMode::Agent, KernelMode::Audited);
    let (spec, _) = derivation
        .derive(&p, SubagentOverrides::default())
        .expect("derivation");
    let default = SubagentBudget::default_for_phase_6a();
    assert_eq!(spec.budget, default);
}

// ---------------------------------------------------------------------
// Property test: narrowing-only invariant.
//
// For ANY (parent_perms, requested_perms) pair drawn from the
// `Capability` universe, `derive(parent, overrides).permissions` must
// be a subset of `parent.permissions`. Equivalently: if derivation
// succeeds, no capability in the output may be absent from the
// parent.
// ---------------------------------------------------------------------

fn arb_capability() -> impl Strategy<Value = Capability> {
    prop_oneof![
        Just(Capability::SpawnAgent),
        Just(Capability::ControlAgent),
        Just(Capability::ReadAgent),
        Just(Capability::ListAgents),
        Just(Capability::InvokeProcess),
        Just(Capability::ReadAllProjects),
        Just(Capability::WriteAllProjects),
    ]
}

fn arb_perms() -> impl Strategy<Value = Permissions> {
    prop::collection::vec(arb_capability(), 0..6).prop_map(|caps| {
        let mut dedup: Vec<Capability> = Vec::new();
        for c in caps {
            if !dedup.contains(&c) {
                dedup.push(c);
            }
        }
        Permissions {
            scope: aura_core_permissions::AgentScope::default(),
            capabilities: dedup,
        }
    })
}

proptest! {
    #[test]
    fn property_derive_permissions_subset_of_parent(
        parent_perms in arb_perms(),
        override_perms in arb_perms(),
    ) {
        let derivation = DefaultDerivation::default();
        let mut p = parent(AgentMode::Agent, KernelMode::Audited);
        p.permissions = parent_perms.clone();
        let result = derivation.derive(
            &p,
            SubagentOverrides {
                permissions: Some(override_perms.clone()),
                ..SubagentOverrides::default()
            },
        );
        match result {
            Ok((spec, _)) => {
                // Every derived capability MUST be satisfied by the
                // parent (`Permissions::contains` checks satisfaction
                // via `Capability::satisfies`, covering wildcard
                // lifting).
                prop_assert!(
                    parent_perms.contains(&spec.permissions),
                    "narrowing invariant violated: parent={:?}, derived={:?}",
                    parent_perms.capabilities,
                    spec.permissions.capabilities,
                );
            }
            Err(DerivationError::PermissionsWiden { capability }) => {
                // Acceptable rejection: the override carried a cap
                // the parent doesn't hold.
                prop_assert!(
                    !parent_perms.capabilities.iter().any(|held| held.satisfies(&capability)),
                    "PermissionsWiden cap {capability:?} was actually held by parent",
                );
            }
            Err(other) => prop_assert!(false, "unexpected error {other:?}"),
        }
    }
}
