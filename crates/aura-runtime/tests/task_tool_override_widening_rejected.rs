//! Phase 7b: every task-tool override field that COULD widen the
//! parent's effective surface must reject with the correct typed
//! [`aura_agent_subagent::DerivationError`] before any child runs.
//!
//! Today, derivation has explicit rejections for:
//!   - `mode` (mode-narrowing table)
//!   - `permissions` (capability subset)
//!   - `kernel_mode` (no downgrade)
//!
//! Tool-subset / model / budget / isolation are admitted by
//! derivation (they don't widen the parent's surface) — they are
//! audit-stamped instead. Those fields are tested for end-to-end
//! plumbing in `task_tool_override_e2e.rs`.

use aura_agent_subagent::{
    DefaultDerivation, DerivationError, ParentContext, SubagentDerivation, SubagentLineage,
    SubagentOverrides,
};
use aura_core::AgentId;
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode};
use aura_core_permissions::{AgentScope, Capability, Permissions};

fn parent_with_caps(caps: Vec<Capability>) -> ParentContext {
    let agent_id = AgentId::generate();
    ParentContext {
        agent_id,
        depth: 0,
        mode: AgentMode::Agent,
        mode_profile: ModeProfile {
            agent: AgentMode::Agent,
            kernel: KernelMode::AuditedLite,
            sandbox: SandboxMode::Standard,
            replay: ReplayMode::Live,
        },
        permissions: Permissions {
            scope: AgentScope::default(),
            capabilities: caps,
        },
        model_id: "claude-opus-4-7".into(),
        lineage: SubagentLineage::from_root(agent_id),
    }
}

#[test]
fn permissions_widen_rejected_with_permissions_widen() {
    let parent = parent_with_caps(vec![Capability::SpawnAgent]);
    let overrides = SubagentOverrides {
        permissions: Some(Permissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent, Capability::ReadAllProjects],
        }),
        ..SubagentOverrides::default()
    };
    let err = DefaultDerivation::default()
        .derive(&parent, overrides)
        .expect_err("widening must be rejected");
    match err {
        DerivationError::PermissionsWiden { capability } => {
            assert_eq!(capability, Capability::ReadAllProjects);
        }
        other => panic!("expected PermissionsWiden, got {other:?}"),
    }
}

#[test]
fn mode_widens_rejected_with_mode_widens() {
    let mut parent = parent_with_caps(vec![Capability::SpawnAgent]);
    parent.mode = AgentMode::Plan;
    parent.mode_profile.agent = AgentMode::Plan;
    let overrides = SubagentOverrides {
        mode: Some(AgentMode::Agent),
        ..SubagentOverrides::default()
    };
    let err = DefaultDerivation::default()
        .derive(&parent, overrides)
        .expect_err("mode widen must be rejected");
    match err {
        DerivationError::ModeWidens { parent, requested } => {
            assert_eq!(parent, AgentMode::Plan);
            assert_eq!(requested, AgentMode::Agent);
        }
        other => panic!("expected ModeWidens, got {other:?}"),
    }
}

#[test]
fn kernel_mode_downgrade_rejected() {
    let mut parent = parent_with_caps(vec![Capability::SpawnAgent]);
    parent.mode_profile.kernel = KernelMode::Audited;
    let overrides = SubagentOverrides {
        kernel_mode: Some(KernelMode::AuditedLite),
        ..SubagentOverrides::default()
    };
    let err = DefaultDerivation::default()
        .derive(&parent, overrides)
        .expect_err("kernel downgrade must be rejected");
    assert!(
        matches!(err, DerivationError::KernelModeDowngradeForbidden { .. }),
        "got {err:?}"
    );
}
