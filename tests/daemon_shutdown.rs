//! Phase 10 — daemon shutdown integration test.
//!
//! Exercises the three documented SIGINT-style shutdown scenarios
//! against an in-process [`FleetDaemon`] driven through the
//! reshaped [`FleetDaemon::run(shutdown)`] entry. Each scenario
//! asserts that a [`aura_store_record::RecordKind::SessionStop`]
//! audit row is written via [`aura_agent_kernel::write_system_record`]
//! with the documented `clean_shutdown` boolean.
//!
//! 1. **Mid-turn cancellation** — a Wait-mode child observes the
//!    daemon-supplied [`CancellationToken`] cancel inside its
//!    `tokio::select!` arm and short-circuits with a
//!    [`SubagentExit::Cancelled`] tag. The daemon's shutdown
//!    sequence emits a `SessionStop { clean_shutdown: true }`.
//!
//! 2. **Mid-tool-call cancellation with grace period** — a slow
//!    child runner is cancelled before its 30 s grace deadline;
//!    the runner finishes inside the cancellation arm of its
//!    `tokio::select!` and the daemon emits
//!    `SessionStop { clean_shutdown: true }`.
//!
//! 3. **Detached children survive as orphans** — a
//!    `SpawnMode::Detached` child writes an orphan record before
//!    its parent's task future is dropped. The orphan is still
//!    readable via [`OrphanStore::list`] after the daemon exits,
//!    and the daemon emits
//!    `SessionStop { clean_shutdown: true }` because the orphan
//!    handoff is the documented clean-shutdown path for detached
//!    children.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_subagent::{ParentContext, SubagentLineage, SubagentOverrides};
use aura_core_modes::{AgentMode, KernelMode, ModeProfile, ReplayMode, SandboxMode, SpawnMode};
use aura_core_permissions::{Capability, Permissions};
use aura_core_types::{AgentId, SubagentExit, SubagentResult, TransactionType};
use aura_fleet_daemon::{
    AgentJob, DaemonConfig, FleetDaemon, SessionRecord, SessionStopRecordPayload,
    RECORD_KIND_SESSION_STOP,
};
use aura_fleet_spawn::{
    ChildRunContext, ChildRunError, ChildRunner, OrphanStore, SpawnHandle, SpawnRequest,
};
use aura_store_db::RocksStore;
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

fn open_test_store(dir: &std::path::Path) -> Arc<dyn aura_store_db::Store> {
    let store = RocksStore::open(dir.join("db"), false).expect("open rocks store");
    Arc::new(store)
}

/// Helper: build a fully-wired [`FleetDaemon`] backed by the supplied
/// runner with an explicit orphan root inside the supplied tempdir.
/// Returns the daemon, the underlying store handle (so the test can
/// scan it for `SessionStop` audit rows after shutdown), and the
/// orphan root path.
fn make_daemon(
    tempdir: &std::path::Path,
    runner: Arc<dyn ChildRunner>,
) -> (
    Arc<FleetDaemon>,
    Arc<dyn aura_store_db::Store>,
    std::path::PathBuf,
) {
    let orphan_root = tempdir.join("orphans");
    let store = open_test_store(tempdir);
    let daemon = Arc::new(FleetDaemon::new(
        store.clone(),
        runner,
        DaemonConfig {
            orphan_root: Some(orphan_root.clone()),
            // Short grace so the test stays fast even when the
            // runner is non-cooperative.
            shutdown_grace: Duration::from_secs(2),
            ..DaemonConfig::default()
        },
    ));
    (daemon, store, orphan_root)
}

/// Scan the agent's record log for a `SessionStop` System audit
/// row and return the parsed payload. Returns `None` if no row was
/// written.
fn find_session_stop(
    store: &Arc<dyn aura_store_db::Store>,
    agent_id: AgentId,
) -> Option<SessionStopRecordPayload> {
    let entries = store.scan_record(agent_id, 1, 200).ok()?;
    for entry in entries {
        if entry.tx.tx_type != TransactionType::System {
            continue;
        }
        let payload: SessionStopRecordPayload = match serde_json::from_slice(&entry.tx.payload) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if payload.kind == RECORD_KIND_SESSION_STOP {
            return Some(payload);
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_in_flight_turn_cancels_cooperatively() {
    // Scenario 1: an in-flight Wait turn observes the daemon's
    // fleet-shutdown cancel-token fire and short-circuits
    // cooperatively. After `run(shutdown)` returns, the audit
    // log contains a `SessionStop { clean_shutdown: true }` row
    // for the registered session.
    let tempdir = tempfile::tempdir().expect("temp dir");
    let runner = Arc::new(CooperativeRunner::new(Duration::from_secs(60)));
    let (daemon, store, _orphans) = make_daemon(tempdir.path(), runner.clone());

    let session_id = "session-mid-turn";
    let parent_ctx = parent_at(AgentMode::Agent);
    let session_agent = parent_ctx.agent_id;
    daemon
        .record_session(SessionRecord {
            session_id: session_id.to_string(),
            agent_id: session_agent,
            total_iterations: 3,
            total_input_tokens: 100,
            total_output_tokens: 50,
            started_at: std::time::Instant::now(),
        })
        .await;

    let shutdown = CancellationToken::new();
    let daemon_for_run = daemon.clone();
    let shutdown_for_run = shutdown.clone();
    let run_handle = tokio::spawn(async move { daemon_for_run.run(shutdown_for_run).await });

    // Spawn a Wait-mode child against the dispatcher so the
    // daemon-owned fleet shutdown token can cancel it when
    // `shutdown.cancel()` fires.
    let dispatcher = daemon.handle().dispatcher();
    let spawn_task = tokio::spawn(async move {
        dispatcher
            .spawn_one(AgentJob {
                request: SpawnRequest {
                    parent: parent_ctx,
                    overrides: SubagentOverrides::default(),
                    prompt: "in-flight-turn".to_string(),
                    originating_user_id: Some("user".to_string()),
                    tool_call_id: None,
                    cancellation: None,
                },
                mode: SpawnMode::Wait,
            })
            .await
    });

    // Wait for the child runner to be in-flight before firing the
    // external shutdown token.
    for _ in 0..200 {
        if runner.invocations() >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(runner.invocations(), 1);
    shutdown.cancel();

    let handle = tokio::time::timeout(Duration::from_secs(10), spawn_task)
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

    tokio::time::timeout(Duration::from_secs(10), run_handle)
        .await
        .expect("daemon.run returns inside the grace window")
        .expect("daemon join")
        .expect("daemon.run ok");

    // Carve-out 2 acceptance: the audit log contains a
    // `SessionStop { clean_shutdown: true }` row for the
    // registered session.
    let session_stop =
        find_session_stop(&store, session_agent).expect("SessionStop audit row written");
    assert!(
        session_stop.clean_shutdown,
        "cooperative cancellation must produce clean_shutdown: true"
    );
    assert_eq!(session_stop.session_id, session_id);
    assert_eq!(session_stop.total_iterations, 3);
    assert_eq!(session_stop.total_input_tokens, 100);
    assert_eq!(session_stop.total_output_tokens, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_during_in_flight_tool_call_uses_grace_period() {
    // Scenario 2: tool-call inside the runner sleeps; cancellation
    // fires inside the documented grace window and the runner
    // unwinds gracefully through its select-on-cancel arm.
    let tempdir = tempfile::tempdir().expect("temp dir");
    let runner = Arc::new(CooperativeRunner::new(Duration::from_secs(60)));
    let (daemon, store, _orphans) = make_daemon(tempdir.path(), runner.clone());

    let parent_ctx = parent_at(AgentMode::Agent);
    let session_agent = parent_ctx.agent_id;
    daemon
        .record_session(SessionRecord {
            session_id: "session-tool-call".to_string(),
            agent_id: session_agent,
            total_iterations: 5,
            total_input_tokens: 0,
            total_output_tokens: 0,
            started_at: std::time::Instant::now(),
        })
        .await;

    let shutdown = CancellationToken::new();
    let daemon_for_run = daemon.clone();
    let shutdown_for_run = shutdown.clone();
    let run_handle = tokio::spawn(async move { daemon_for_run.run(shutdown_for_run).await });

    let dispatcher = daemon.handle().dispatcher();
    let spawn_task = tokio::spawn(async move {
        dispatcher
            .spawn_one(AgentJob {
                request: SpawnRequest {
                    parent: parent_ctx,
                    overrides: SubagentOverrides::default(),
                    prompt: "in-flight-tool-call".to_string(),
                    originating_user_id: Some("user".to_string()),
                    tool_call_id: None,
                    cancellation: None,
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
    shutdown.cancel();

    let handle = tokio::time::timeout(Duration::from_secs(10), spawn_task)
        .await
        .expect("spawn_one returns inside the grace window")
        .expect("join handle")
        .expect("spawn_one ok");
    let drained_in = cancel_at.elapsed();
    assert!(
        drained_in < Duration::from_secs(30),
        "child must observe cancel inside grace window (drained_in={drained_in:?})"
    );
    let SpawnHandle::Completed(result) = handle else {
        panic!("Wait mode returns SpawnHandle::Completed");
    };
    assert!(matches!(result.exit, SubagentExit::Cancelled));
    assert_eq!(runner.cancelled_invocations(), 1);

    tokio::time::timeout(Duration::from_secs(10), run_handle)
        .await
        .expect("daemon.run returns inside the grace window")
        .expect("daemon join")
        .expect("daemon.run ok");

    let session_stop =
        find_session_stop(&store, session_agent).expect("SessionStop audit row written");
    assert!(
        session_stop.clean_shutdown,
        "grace-window settle must produce clean_shutdown: true"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigint_with_detached_children_alive_persists_orphans_and_is_reapable() {
    // Scenario 3: a `SpawnMode::Detached` child writes an orphan
    // record. After daemon shutdown the orphan record persists so
    // `aura agents reap` can clean it up; the audit log carries a
    // `SessionStop { clean_shutdown: true }` row.
    let tempdir = tempfile::tempdir().expect("temp dir");
    let runner = Arc::new(CooperativeRunner::new(Duration::from_secs(30)));
    let (daemon, store, orphan_root) = make_daemon(tempdir.path(), runner.clone());

    let parent_ctx = parent_at(AgentMode::Agent);
    let session_agent = parent_ctx.agent_id;
    daemon
        .record_session(SessionRecord {
            session_id: "session-detached".to_string(),
            agent_id: session_agent,
            total_iterations: 1,
            total_input_tokens: 0,
            total_output_tokens: 0,
            started_at: std::time::Instant::now(),
        })
        .await;

    let shutdown = CancellationToken::new();
    let daemon_for_run = daemon.clone();
    let shutdown_for_run = shutdown.clone();
    let run_handle = tokio::spawn(async move { daemon_for_run.run(shutdown_for_run).await });

    let dispatcher = daemon.handle().dispatcher();
    let handle = dispatcher
        .spawn_one(AgentJob {
            request: SpawnRequest {
                parent: parent_ctx,
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

    // The spawner writes the orphan record before returning from
    // the detached path.
    let inspect_store = OrphanStore::new(orphan_root.clone());
    let orphan = inspect_store
        .load(detached.agent_id)
        .expect("orphan store io")
        .expect("orphan record exists for detached child");
    assert_eq!(orphan.spawn_mode, SpawnMode::Detached);
    assert_eq!(orphan.mode, AgentMode::Agent);
    assert_eq!(orphan.agent_id, detached.agent_id);

    drop(detached);

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), run_handle)
        .await
        .expect("daemon.run returns inside the grace window")
        .expect("daemon join")
        .expect("daemon.run ok");

    // Orphan record persists across daemon shutdown.
    let post = inspect_store.list().expect("list orphans post-shutdown");
    assert_eq!(
        post.len(),
        1,
        "orphan record must survive parent death so `aura agents reap` can clean it up"
    );

    // Carve-out 2 acceptance.
    let session_stop =
        find_session_stop(&store, session_agent).expect("SessionStop audit row written");
    assert!(
        session_stop.clean_shutdown,
        "detached children handed to orphan store is the documented clean-shutdown path"
    );

    // Reap via the public OrphanStore surface (the same one
    // `aura agents reap` calls). Idempotency check.
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

    assert!(runner.invocations() >= 1);
}
