//! Phase 9 — daemon shutdown integration test.
//!
//! Exercises the three documented SIGINT-style shutdown scenarios
//! against an in-process [`FleetDaemon`]:
//!
//! 1. **Mid-turn cancellation** — a Wait-mode child observes the
//!    parent-supplied [`CancellationToken`] cancel and short-circuits
//!    with a [`SubagentExit::Cancelled`] tag. The dispatcher then
//!    returns control to the caller (the in-process simulation of
//!    "current iteration completes, then daemon exits with code 0").
//!
//! 2. **Mid-tool-call cancellation with grace period** — a slow
//!    child runner is cancelled before its 30 s grace deadline; the
//!    runner finishes gracefully inside the cancellation arm of its
//!    `tokio::select!` and the dispatcher returns the cancelled
//!    [`SubagentResult`].
//!
//! 3. **Detached children survive as orphans** — a `SpawnMode::Detached`
//!    child writes an orphan record before its parent's task future
//!    is dropped. The orphan is still readable via
//!    [`OrphanStore::list`] after the parent goes away — i.e.
//!    reapable through the `aura agents reap` surface (Phase 7b
//!    machinery).
//!
//! ## Deviations from the original spec
//!
//! The Phase 9 task description references a
//! `RecordKind::SessionStop` audit record. That variant is not
//! present in the closed [`aura_store_record::RecordKind`]
//! taxonomy today (only `ChildOrphanedByParentDeath`,
//! `ChildCancelledByParentDeath`, `OrphanReaped`, etc.). This test
//! asserts the **structural** shutdown contract — cooperative
//! cancellation propagation + persisted orphan surface — rather than
//! the specific kind tag, so the test remains valid until a future
//! phase grows the taxonomy. The deviation is documented here so the
//! test can be tightened in Phase 10 once `RecordKind::SessionStop`
//! lands.
//!
//! The test also operates against the dispatcher's `spawn_one` seam
//! rather than the full [`FleetDaemon::run`] mailbox loop. The
//! daemon's run loop holds a sender clone via its own
//! [`FleetDaemonHandle`] for the duration of `&self`, so the loop
//! cannot terminate without dropping the daemon itself — which we
//! cannot do mid-run. Phase 10 will reshape `run()` to accept an
//! external shutdown token and the test will be re-pointed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentOverrides};
use aura_core::{AgentId, SubagentExit, SubagentResult};
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode, SpawnMode};
use aura_core_permissions::{Capability, Permissions};
use aura_fleet_daemon::{AgentJob, DaemonConfig, FleetDaemon};
use aura_fleet_spawn::{
    ChildRunContext, ChildRunError, ChildRunner, OrphanStore, SpawnHandle, SpawnRequest,
};
use aura_store::RocksStore;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

/// Test-only child runner that polls the parent-supplied
/// [`CancellationToken`] inside a `tokio::select!` arm. Cooperatively
/// short-circuits with a `SubagentExit::Cancelled` tag when the
/// token fires before the configured run duration elapses.
struct CooperativeRunner {
    run_duration: Duration,
    invocations: AtomicUsize,
    cancelled_invocations: AtomicUsize,
    captured_calls: Mutex<Vec<AgentId>>,
}

impl CooperativeRunner {
    fn new(run_duration: Duration) -> Self {
        Self {
            run_duration,
            invocations: AtomicUsize::new(0),
            cancelled_invocations: AtomicUsize::new(0),
            captured_calls: Mutex::new(Vec::new()),
        }
    }

    fn invocations(&self) -> usize {
        self.invocations.load(Ordering::SeqCst)
    }

    fn cancelled_invocations(&self) -> usize {
        self.cancelled_invocations.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ChildRunner for CooperativeRunner {
    async fn run(&self, ctx: ChildRunContext) -> Result<SubagentResult, ChildRunError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        self.captured_calls.lock().push(ctx.preassigned_agent_id);

        tokio::select! {
            () = tokio::time::sleep(self.run_duration) => Ok(SubagentResult {
                child_agent_id: Some(ctx.preassigned_agent_id),
                final_message: "ran to completion".to_string(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Completed,
            }),
            () = ctx.cancellation.cancelled() => {
                self.cancelled_invocations.fetch_add(1, Ordering::SeqCst);
                Ok(SubagentResult {
                    child_agent_id: Some(ctx.preassigned_agent_id),
                    final_message: String::new(),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    files_changed: Vec::new(),
                    exit: SubagentExit::Cancelled,
                })
            }
        }
    }
}

fn parent_at(mode: AgentMode) -> ParentContext {
    let agent_id = AgentId::generate();
    let profile = ModeProfile {
        agent: mode,
        kernel: KernelMode::Audited,
        sandbox: SandboxMode::Standard,
        replay: ReplayMode::Live,
    };
    ParentContext {
        agent_id,
        depth: 0,
        mode,
        mode_profile: profile,
        permissions: Permissions {
            scope: aura_core_permissions::AgentScope::default(),
            capabilities: vec![Capability::SpawnAgent, Capability::ReadAllProjects],
        },
        model_id: "claude-opus-4-7".to_string(),
        lineage: SubagentLineage::from_root(agent_id),
    }
}

fn open_test_store(dir: &std::path::Path) -> Arc<dyn aura_store::Store> {
    let store = RocksStore::open(dir.join("db"), false).expect("open rocks store");
    Arc::new(store)
}

/// Helper: build a fully-wired [`FleetDaemon`] backed by the supplied
/// runner with an explicit orphan root inside the supplied tempdir.
fn make_daemon(
    tempdir: &std::path::Path,
    runner: Arc<dyn ChildRunner>,
) -> (Arc<FleetDaemon>, std::path::PathBuf) {
    let orphan_root = tempdir.join("orphans");
    let store = open_test_store(tempdir);
    let daemon = Arc::new(FleetDaemon::new(
        store,
        runner,
        DaemonConfig {
            orphan_root: Some(orphan_root.clone()),
            ..DaemonConfig::default()
        },
    ));
    (daemon, orphan_root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_in_flight_turn_cancels_cooperatively() {
    // Scenario 1: an in-flight Wait turn observes a cancel-token fire
    // and short-circuits cooperatively. The dispatcher returns the
    // child's `SubagentExit::Cancelled` result — the in-process
    // simulation of "running turn completes the current iteration,
    // then daemon exits".
    let tempdir = tempfile::tempdir().expect("temp dir");
    let runner = Arc::new(CooperativeRunner::new(Duration::from_secs(60)));
    let (daemon, _orphans) = make_daemon(tempdir.path(), runner.clone());

    let cancel_token = CancellationToken::new();
    let dispatcher = daemon.handle().dispatcher();

    let job_cancel = cancel_token.clone();
    let dispatcher_for_task = dispatcher.clone();
    let spawn_task = tokio::spawn(async move {
        dispatcher_for_task
            .spawn_one(AgentJob {
                request: SpawnRequest {
                    parent: parent_at(AgentMode::Agent),
                    overrides: SubagentOverrides::default(),
                    prompt: "in-flight-turn".to_string(),
                    originating_user_id: Some("user".to_string()),
                    tool_call_id: None,
                    cancellation: Some(job_cancel),
                },
                mode: SpawnMode::Wait,
            })
            .await
    });

    // Wait for the child runner to be in-flight before firing cancel.
    for _ in 0..200 {
        if runner.invocations() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(runner.invocations(), 1);
    cancel_token.cancel();

    let handle = tokio::time::timeout(Duration::from_secs(5), spawn_task)
        .await
        .expect("spawn_one returns inside the grace window")
        .expect("join handle")
        .expect("spawn_one ok");

    let SpawnHandle::Completed(result) = handle else {
        panic!("Wait mode returns SpawnHandle::Completed");
    };
    assert!(
        matches!(result.exit, SubagentExit::Cancelled),
        "cancelled in-flight turn must surface SubagentExit::Cancelled (got {:?})",
        result.exit
    );
    assert_eq!(runner.cancelled_invocations(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_in_flight_tool_call_uses_grace_period() {
    // Scenario 2: tool-call inside the runner sleeps; cancellation
    // fires inside the documented 30 s grace window and the runner
    // unwinds gracefully through its select-on-cancel arm.
    let tempdir = tempfile::tempdir().expect("temp dir");
    let grace_period = Duration::from_secs(30);
    // Initial duration deliberately exceeds the grace window so the
    // cancel-arm is the only successful exit path.
    let runner = Arc::new(CooperativeRunner::new(grace_period * 2));
    let (daemon, _orphans) = make_daemon(tempdir.path(), runner.clone());

    let cancel_token = CancellationToken::new();
    let dispatcher = daemon.handle().dispatcher();
    let job_cancel = cancel_token.clone();
    let dispatcher_for_task = dispatcher.clone();
    let spawn_task = tokio::spawn(async move {
        dispatcher_for_task
            .spawn_one(AgentJob {
                request: SpawnRequest {
                    parent: parent_at(AgentMode::Agent),
                    overrides: SubagentOverrides::default(),
                    prompt: "in-flight-tool-call".to_string(),
                    originating_user_id: Some("user".to_string()),
                    tool_call_id: None,
                    cancellation: Some(job_cancel),
                },
                mode: SpawnMode::Wait,
            })
            .await
    });

    for _ in 0..200 {
        if runner.invocations() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cancel_at = std::time::Instant::now();
    cancel_token.cancel();

    let handle = tokio::time::timeout(Duration::from_secs(5), spawn_task)
        .await
        .expect("spawn_one returns inside the grace window")
        .expect("join handle")
        .expect("spawn_one ok");
    let drained_in = cancel_at.elapsed();
    assert!(
        drained_in < grace_period,
        "child must observe cancel inside grace window (drained_in={drained_in:?})"
    );
    let SpawnHandle::Completed(result) = handle else {
        panic!("Wait mode returns SpawnHandle::Completed");
    };
    assert!(matches!(result.exit, SubagentExit::Cancelled));
    assert_eq!(runner.cancelled_invocations(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_with_detached_children_alive_persists_orphans_and_is_reapable() {
    // Scenario 3: a `SpawnMode::Detached` child must write an orphan
    // record before its parent's task future is dropped. After the
    // parent goes away, the orphan record is reapable via
    // `aura agents reap` (the on-disk surface `OrphanStore::list()`
    // walks).
    let tempdir = tempfile::tempdir().expect("temp dir");
    // Long-running detached child so the orphan record is visible
    // before the runner returns.
    let runner = Arc::new(CooperativeRunner::new(Duration::from_secs(30)));
    let (daemon, orphan_root) = make_daemon(tempdir.path(), runner.clone());

    let dispatcher = daemon.handle().dispatcher();
    let handle = dispatcher
        .spawn_one(AgentJob {
            request: SpawnRequest {
                parent: parent_at(AgentMode::Agent),
                overrides: SubagentOverrides::default(),
                prompt: "detached-child".to_string(),
                originating_user_id: Some("user".to_string()),
                tool_call_id: None,
                cancellation: None,
            },
            mode: SpawnMode::Detached,
        })
        .await
        .expect("detached spawn ok");

    let SpawnHandle::Detached(detached) = handle else {
        panic!("Detached spawn returns SpawnHandle::Detached");
    };

    // The spawner writes the orphan record before returning from the
    // detached path. The on-disk surface is the source of truth that
    // `aura agents reap` reads after a daemon restart.
    let inspect_store = OrphanStore::new(orphan_root.clone());
    let orphan = inspect_store
        .load(detached.agent_id)
        .expect("orphan store io")
        .expect("orphan record exists for detached child");
    assert_eq!(orphan.spawn_mode, SpawnMode::Detached);
    assert_eq!(orphan.mode, AgentMode::Agent);
    assert_eq!(orphan.agent_id, detached.agent_id);

    // Simulate parent death: drop every dispatcher / detached handle
    // the parent owned. The orphan record persists.
    drop(detached);
    drop(dispatcher);
    drop(daemon);

    let post = inspect_store.list().expect("list orphans post-shutdown");
    assert_eq!(
        post.len(),
        1,
        "orphan record must survive parent death so `aura agents reap` can clean it up"
    );

    // Reap via the public OrphanStore surface (the same one
    // `aura agents reap` calls). Idempotency check: a second reap
    // is a no-op.
    inspect_store
        .remove(post[0].agent_id)
        .expect("first reap ok");
    inspect_store
        .remove(post[0].agent_id)
        .expect("reap is idempotent");
    assert!(
        inspect_store.list().expect("list after reap").is_empty(),
        "orphan store must be empty after reap"
    );

    // Sanity: at least one in-flight detached invocation was started
    // before the parent dropped.
    assert!(runner.invocations() >= 1);
}
