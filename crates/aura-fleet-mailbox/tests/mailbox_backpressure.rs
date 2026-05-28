//! Phase 7b: a saturated mailbox with a deadlined send returns
//! [`MailboxError::Backpressured`]; once a slot frees the next send
//! resolves normally.

use std::time::Duration;

use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentOverrides};
use aura_core::AgentId;
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode, SpawnMode};
use aura_core_permissions::{AgentScope, Capability, Permissions};
use aura_fleet_dispatch::AgentJob;
use aura_fleet_mailbox::{Mailbox, MailboxConfig, MailboxError};
use aura_fleet_spawn::SpawnRequest;

fn make_job() -> AgentJob {
    let agent_id = AgentId::generate();
    let parent = ParentContext {
        agent_id,
        depth: 0,
        mode: AgentMode::Agent,
        mode_profile: ModeProfile {
            agent: AgentMode::Agent,
            kernel: KernelMode::Audited,
            sandbox: SandboxMode::Standard,
            replay: ReplayMode::Live,
        },
        permissions: Permissions {
            scope: AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent],
        },
        model_id: "claude-opus-4-7".into(),
        lineage: SubagentLineage::from_root(agent_id),
    };
    AgentJob {
        request: SpawnRequest {
            parent,
            overrides: SubagentOverrides::default(),
            prompt: "hello".into(),
            originating_user_id: Some("user".into()),
            tool_call_id: None,
            cancellation: None,
        },
        mode: SpawnMode::Wait,
    }
}

#[tokio::test]
async fn full_mailbox_with_deadline_returns_backpressured() {
    let mailbox = Mailbox::with_config(MailboxConfig { capacity: 1 });
    let (sender, mut receiver) = mailbox.into_parts();

    sender.send(make_job()).await.expect("first send fits");

    let err = sender
        .send_with_deadline(make_job(), Duration::from_millis(50))
        .await
        .expect_err("second send must Backpressure");
    match err {
        MailboxError::Backpressured {
            capacity,
            deadline_ms,
        } => {
            assert_eq!(capacity, 1);
            assert!(deadline_ms >= 40);
        }
        other => panic!("expected Backpressured, got {other:?}"),
    }

    // Draining unblocks subsequent sends.
    let _drained = receiver.recv().await.expect("drain");
    sender
        .send_with_deadline(make_job(), Duration::from_millis(50))
        .await
        .expect("post-drain send fits");
}

#[tokio::test]
async fn closed_receiver_surfaces_closed_error() {
    let mailbox = Mailbox::with_config(MailboxConfig { capacity: 1 });
    let (sender, receiver) = mailbox.into_parts();
    drop(receiver);
    let err = sender.send(make_job()).await.expect_err("must error");
    assert!(matches!(err, MailboxError::Closed));
}
