//! Tests for the automaton bridge.
//!
//! Two clusters live here:
//!
//! 1. Lifecycle/inbox tests that exercise
//!    [`AutomatonBridge::record_lifecycle_event`] and
//!    [`AutomatonBridge::prepare_installed_tools`] (Invariants §2, §8
//!    + the brave-search regression).
//! 2. Event-stream replay tests (`late_subscriber_*`,
//!    `mid_stream_subscriber_*`, `history_caps_*`) that drive
//!    `spawn_event_forwarder` directly via its `mpsc` channel and
//!    assert the late-WS-subscriber race no longer drops the terminal
//!    event. Those tests reach back into the channel-side constants
//!    via `super::event_channel::EVENT_HISTORY_CAPACITY`.

use super::event_channel::EVENT_HISTORY_CAPACITY;
use super::{AutomatonBridge, Scheduler};
use async_trait::async_trait;
use aura_automaton::AutomatonRuntime;
use aura_core::{AgentId, InstalledIntegrationDefinition, TransactionType};
use aura_reasoner::{MockProvider, ModelProvider};
use aura_store::{RocksStore, Store};
use aura_tools::{
    domain_tools::{
        CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
        SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
    },
    ToolCatalog, ToolConfig,
};
use std::sync::Arc;

/// A `DomainApi` stub whose methods all panic — the lifecycle test
/// below never invokes any of them because it only exercises the
/// bridge's inbox/scheduler wiring, not the automaton runtime
/// itself.
struct UnusedDomain;

#[async_trait]
impl DomainApi for UnusedDomain {
    async fn list_specs(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        unimplemented!("UnusedDomain")
    }
    async fn get_spec(&self, _spec_id: &str, _jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn create_spec(
        &self,
        _p: &str,
        _t: &str,
        _c: &str,
        _o: u32,
        _j: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn update_spec(
        &self,
        _id: &str,
        _t: Option<&str>,
        _c: Option<&str>,
        _j: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn delete_spec(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<()> {
        unimplemented!("UnusedDomain")
    }
    async fn list_tasks(
        &self,
        _p: &str,
        _s: Option<&str>,
        _j: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        unimplemented!("UnusedDomain")
    }
    async fn create_task(
        &self,
        _p: &str,
        _s: &str,
        _t: &str,
        _d: &str,
        _deps: &[String],
        _o: u32,
        _j: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn update_task(
        &self,
        _id: &str,
        _u: TaskUpdate,
        _j: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn delete_task(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<()> {
        unimplemented!("UnusedDomain")
    }
    async fn transition_task(
        &self,
        _id: &str,
        _s: &str,
        _j: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn claim_next_task(
        &self,
        _p: &str,
        _a: &str,
        _j: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        unimplemented!("UnusedDomain")
    }
    async fn get_task(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn get_project(&self, _p: &str, _j: Option<&str>) -> anyhow::Result<ProjectDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn update_project(
        &self,
        _p: &str,
        _u: ProjectUpdate,
        _j: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn create_log(
        &self,
        _p: &str,
        _m: &str,
        _l: &str,
        _a: Option<&str>,
        _md: Option<&serde_json::Value>,
        _j: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        unimplemented!("UnusedDomain")
    }
    async fn list_logs(
        &self,
        _p: &str,
        _l: Option<&str>,
        _n: Option<u64>,
        _j: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        unimplemented!("UnusedDomain")
    }
    async fn get_project_stats(
        &self,
        _p: &str,
        _j: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        unimplemented!("UnusedDomain")
    }
    async fn list_messages(&self, _p: &str, _i: &str) -> anyhow::Result<Vec<MessageDescriptor>> {
        unimplemented!("UnusedDomain")
    }
    async fn save_message(&self, _p: SaveMessageParams) -> anyhow::Result<()> {
        unimplemented!("UnusedDomain")
    }
    async fn create_session(&self, _p: CreateSessionParams) -> anyhow::Result<SessionDescriptor> {
        unimplemented!("UnusedDomain")
    }
    async fn get_active_session(&self, _i: &str) -> anyhow::Result<Option<SessionDescriptor>> {
        unimplemented!("UnusedDomain")
    }
    async fn orbit_api_call(
        &self,
        _m: &str,
        _p: &str,
        _b: Option<&serde_json::Value>,
        _j: Option<&str>,
    ) -> anyhow::Result<String> {
        unimplemented!("UnusedDomain")
    }
    async fn network_api_call(
        &self,
        _m: &str,
        _p: &str,
        _b: Option<&serde_json::Value>,
        _j: Option<&str>,
    ) -> anyhow::Result<String> {
        unimplemented!("UnusedDomain")
    }
}

fn count_lifecycle_entries(store: &Arc<dyn Store>, agent_id: AgentId) -> usize {
    store
        .scan_record(agent_id, 0, 256)
        .expect("scan_record")
        .into_iter()
        .filter(|e| e.tx.tx_type == TransactionType::System)
        .filter(|e| {
            serde_json::from_slice::<serde_json::Value>(&e.tx.payload)
                .ok()
                .and_then(|v| {
                    v.get("system_kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("automaton_lifecycle")
        })
        .count()
}

/// §2 + §8: starting and stopping an automaton must each produce
/// one `System::AutomatonLifecycle` entry in the record log for the
/// owning agent. This test exercises the bridge's
/// `record_lifecycle_event` seam directly so the assertion is
/// focused on the inbox → scheduler → record-log hop that the
/// automaton runtime triggers, without spinning up a real dev loop.
#[tokio::test]
async fn start_then_stop_records_two_automaton_lifecycle_entries() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));
    let ws_dir = dir.path().join("workspaces");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        ws_dir,
        None,
    ));

    let runtime = Arc::new(AutomatonRuntime::new());
    let catalog = Arc::new(ToolCatalog::new());
    let domain: Arc<dyn DomainApi> = Arc::new(UnusedDomain);
    let bridge = AutomatonBridge::new(
        runtime,
        store.clone(),
        domain,
        provider,
        catalog,
        ToolConfig::default(),
    )
    .with_scheduler(scheduler);

    let agent_id = AgentId::generate();

    // The post-fix `Scheduler::schedule_agent` requires the agent's
    // identity to be registered before it will dispatch a tick. The
    // production dev-loop / task-run kickoff paths register identity
    // in `register_automaton_identity` (called from
    // `dispatch.rs::start_dev_loop_with_capabilities`); this test
    // exercises `record_lifecycle_event` directly so we register
    // here to mirror the production flow without spinning up a full
    // dev loop.
    bridge.register_automaton_identity(
        agent_id,
        "claude-test-model",
        None,
        None,
        None,
        None,
        None,
        aura_reasoner::ModelRequestKind::DevLoopBootstrap,
    );

    bridge
        .record_lifecycle_event(agent_id, "aut-1", "start_dev_loop")
        .await;
    bridge
        .record_lifecycle_event(agent_id, "aut-1", "stop_dev_loop")
        .await;

    let count = count_lifecycle_entries(&store, agent_id);
    assert_eq!(
        count, 2,
        "expected exactly 2 System/AutomatonLifecycle entries, got {count}"
    );
}

#[test]
fn prepare_installed_tools_filters_by_required_integration() {
    let tools = AutomatonBridge::prepare_installed_tools(
        Some(vec![
            aura_protocol::InstalledTool {
                name: "brave_search_web".to_string(),
                description: "Search the web using Brave".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"]
                }),
                endpoint: "https://example.com/brave".to_string(),
                auth: aura_protocol::ToolAuth::None,
                timeout_ms: None,
                namespace: None,
                required_integration: Some(aura_protocol::InstalledToolIntegrationRequirement {
                    integration_id: None,
                    provider: Some("brave_search".to_string()),
                    kind: Some("workspace_integration".to_string()),
                }),
                runtime_execution: None,
                metadata: Default::default(),
            },
            aura_protocol::InstalledTool {
                name: "list_org_integrations".to_string(),
                description: "List org integrations".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
                endpoint: "https://example.com/list".to_string(),
                auth: aura_protocol::ToolAuth::None,
                timeout_ms: None,
                namespace: None,
                required_integration: None,
                runtime_execution: None,
                metadata: Default::default(),
            },
        ]),
        &[InstalledIntegrationDefinition {
            integration_id: "brave-1".to_string(),
            name: "Brave Search".to_string(),
            provider: "brave_search".to_string(),
            kind: "workspace_integration".to_string(),
            metadata: Default::default(),
        }],
    );

    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"brave_search_web"));
    assert!(names.contains(&"list_org_integrations"));

    let filtered = AutomatonBridge::prepare_installed_tools(
        Some(vec![aura_protocol::InstalledTool {
            name: "brave_search_web".to_string(),
            description: "Search the web using Brave".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
            endpoint: "https://example.com/brave".to_string(),
            auth: aura_protocol::ToolAuth::None,
            timeout_ms: None,
            namespace: None,
            required_integration: Some(aura_protocol::InstalledToolIntegrationRequirement {
                integration_id: None,
                provider: Some("brave_search".to_string()),
                kind: Some("workspace_integration".to_string()),
            }),
            runtime_execution: None,
            metadata: Default::default(),
        }]),
        &[],
    );

    assert!(filtered.is_empty());
}

// ------------------------------------------------------------------
// Event-stream replay tests
//
// Regression tests for the race described on [`EventChannel`]:
// `aura-os-server` connects to `/stream/automaton/:id` *after*
// `POST /automaton/start` returns. `tokio::sync::broadcast`
// receivers only observe events sent after they subscribe, so a
// fast-terminating automaton used to look like "stream closed
// without a terminal event" from the server's point of view.
//
// These tests drive `spawn_event_forwarder` directly via the mpsc
// it consumes, then exercise `subscribe_events` as a late
// subscriber.
// ------------------------------------------------------------------

fn test_bridge() -> AutomatonBridge {
    use crate::scheduler::Scheduler;
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));
    let ws_dir = dir.path().join("workspaces");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider.clone(),
        vec![],
        vec![],
        ws_dir,
        None,
    ));
    let runtime = Arc::new(AutomatonRuntime::new());
    let catalog = Arc::new(ToolCatalog::new());
    let domain: Arc<dyn DomainApi> = Arc::new(UnusedDomain);
    AutomatonBridge::new(
        runtime,
        store,
        domain,
        provider,
        catalog,
        ToolConfig::default(),
    )
    .with_scheduler(scheduler)
}

/// A subscriber that joins after every event has been emitted still
/// sees the full sequence via [`EventSubscription::history`].
#[tokio::test]
async fn late_subscriber_sees_replayed_history_after_done() {
    use aura_automaton::AutomatonEvent;

    let bridge = test_bridge();
    let automaton_id = "aut-replay".to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    bridge.spawn_event_forwarder(automaton_id.clone(), rx);

    tx.send(AutomatonEvent::Started {
        automaton_id: automaton_id.clone(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::TaskStarted {
        task_id: "task-1".into(),
        task_title: "first task".into(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::TaskFailed {
        task_id: "task-1".into(),
        reason: "boom".into(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::Stopped {
        automaton_id: automaton_id.clone(),
        reason: "Failed".into(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::Done).await.unwrap();

    // Wait for the forwarder to observe Done and set `done=true`.
    // The forwarder pushes history before toggling the flag, so
    // once `already_done` is true we know every event is visible.
    let subscription = loop {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let sub = bridge
            .subscribe_events(&automaton_id)
            .expect("channel still in retention window");
        if sub.already_done {
            break sub;
        }
    };

    let kinds: Vec<&'static str> = subscription
        .history
        .iter()
        .map(|e| match e {
            AutomatonEvent::Started { .. } => "started",
            AutomatonEvent::TaskStarted { .. } => "task_started",
            AutomatonEvent::TaskFailed { .. } => "task_failed",
            AutomatonEvent::Stopped { .. } => "stopped",
            AutomatonEvent::Done => "done",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["started", "task_started", "task_failed", "stopped", "done"],
        "late subscriber must see every emitted event in order"
    );
    assert!(subscription.already_done);
}

/// A subscriber that joins mid-stream sees the events emitted so
/// far through `history` and any later events through `live`, in
/// order. This is what would have saved us in the logs the user
/// shared: the WS would observe `Started → TaskFailed → Done`
/// regardless of whether it subscribed 1 ms or 200 ms after
/// `POST /automaton/start` returned.
#[tokio::test]
async fn mid_stream_subscriber_sees_history_then_live_events() {
    use aura_automaton::AutomatonEvent;

    let bridge = test_bridge();
    let automaton_id = "aut-mid".to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    bridge.spawn_event_forwarder(automaton_id.clone(), rx);

    tx.send(AutomatonEvent::Started {
        automaton_id: automaton_id.clone(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::TaskStarted {
        task_id: "task-1".into(),
        task_title: "first".into(),
    })
    .await
    .unwrap();

    // Let the forwarder drain the two events above into history.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let mut subscription = bridge
        .subscribe_events(&automaton_id)
        .expect("channel present");
    assert!(!subscription.already_done);
    assert_eq!(subscription.history.len(), 2);
    assert!(matches!(
        subscription.history[0],
        AutomatonEvent::Started { .. }
    ));
    assert!(matches!(
        subscription.history[1],
        AutomatonEvent::TaskStarted { .. }
    ));

    // Emit the remainder after subscribe. These should arrive on
    // the live receiver, not in history (history was snapshotted).
    tx.send(AutomatonEvent::TaskCompleted {
        task_id: "task-1".into(),
        summary: "ok".into(),
    })
    .await
    .unwrap();
    tx.send(AutomatonEvent::Done).await.unwrap();

    let first = subscription.live.recv().await.expect("live event");
    assert!(matches!(first, AutomatonEvent::TaskCompleted { .. }));
    let second = subscription.live.recv().await.expect("live event");
    assert!(matches!(second, AutomatonEvent::Done));
}

/// History is capped at [`EVENT_HISTORY_CAPACITY`] so long-lived
/// dev-loop automatons don't grow unbounded. The oldest events
/// are dropped first; this is consistent with how
/// `tokio::sync::broadcast` would have behaved for an early
/// subscriber that fell behind.
#[tokio::test]
async fn history_caps_at_capacity_and_drops_oldest() {
    use aura_automaton::AutomatonEvent;

    let bridge = test_bridge();
    let automaton_id = "aut-cap".to_string();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    bridge.spawn_event_forwarder(automaton_id.clone(), rx);

    let over = EVENT_HISTORY_CAPACITY + 5;
    for i in 0..over {
        tx.send(AutomatonEvent::LogLine {
            message: format!("line {i}"),
        })
        .await
        .unwrap();
    }

    // Drain.
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;

    let subscription = bridge
        .subscribe_events(&automaton_id)
        .expect("channel present");
    assert_eq!(
        subscription.history.len(),
        EVENT_HISTORY_CAPACITY,
        "history must be capped at EVENT_HISTORY_CAPACITY"
    );
    // The very first 5 "line 0".."line 4" should have been evicted.
    match &subscription.history[0] {
        AutomatonEvent::LogLine { message } => {
            assert_eq!(
                message, "line 5",
                "oldest surviving entry should be the 6th emitted event"
            );
        }
        other => panic!("unexpected event kind: {other:?}"),
    }
}
