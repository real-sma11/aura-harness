use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_core_types::{AgentId, TransactionType};
use aura_model_reasoner::{MockProvider, ModelProvider};
use aura_store_db::{RocksStore, Store};
use aura_tools::domain_tools::{
    CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
    SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
};
use serde_json::{json, Value};
use tempfile::TempDir;

use super::KernelDomainGateway;

// ---- Test double for `DomainApi` ---------------------------------

#[derive(Default)]
struct MockDomain {
    call_log: Mutex<Vec<&'static str>>,
    list_tasks_calls: AtomicUsize,
    fail_create_spec: bool,
}

impl MockDomain {
    fn new() -> Self {
        Self::default()
    }
    fn with_failing_create_spec() -> Self {
        Self {
            fail_create_spec: true,
            ..Self::default()
        }
    }
    fn record(&self, name: &'static str) {
        self.call_log.lock().unwrap().push(name);
    }
}

#[async_trait]
impl DomainApi for MockDomain {
    async fn list_specs(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        self.record("list_specs");
        Ok(vec![])
    }
    async fn get_spec(&self, spec_id: &str, _jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        self.record("get_spec");
        Ok(SpecDescriptor {
            id: spec_id.to_string(),
            project_id: "p".to_string(),
            title: "t".to_string(),
            content: String::new(),
            order: 0,
            parent_id: None,
            content_hash: None,
        })
    }
    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        _jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.record("create_spec");
        if self.fail_create_spec {
            anyhow::bail!("simulated domain failure");
        }
        Ok(SpecDescriptor {
            id: "new-spec".to_string(),
            project_id: project_id.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            order,
            parent_id: None,
            content_hash: None,
        })
    }
    async fn update_spec(
        &self,
        spec_id: &str,
        _title: Option<&str>,
        _content: Option<&str>,
        _if_match: Option<&str>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        self.record("update_spec");
        Ok(SpecDescriptor {
            id: spec_id.to_string(),
            project_id: "p".to_string(),
            title: "t".to_string(),
            content: String::new(),
            order: 0,
            parent_id: None,
            content_hash: None,
        })
    }
    async fn delete_spec(&self, _spec_id: &str, _jwt: Option<&str>) -> anyhow::Result<()> {
        self.record("delete_spec");
        Ok(())
    }
    async fn list_tasks(
        &self,
        _project_id: &str,
        _spec_id: Option<&str>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        self.list_tasks_calls.fetch_add(1, Ordering::SeqCst);
        self.record("list_tasks");
        Ok(vec![])
    }
    async fn create_task(
        &self,
        project_id: &str,
        spec_id: &str,
        title: &str,
        description: &str,
        _dependencies: &[String],
        order: u32,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.record("create_task");
        Ok(TaskDescriptor {
            id: "t1".into(),
            spec_id: spec_id.into(),
            project_id: project_id.into(),
            title: title.into(),
            description: description.into(),
            status: "open".into(),
            dependencies: vec![],
            order,
        })
    }
    async fn update_task(
        &self,
        task_id: &str,
        _updates: TaskUpdate,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.record("update_task");
        Ok(TaskDescriptor {
            id: task_id.into(),
            spec_id: String::new(),
            project_id: String::new(),
            title: String::new(),
            description: String::new(),
            status: "open".into(),
            dependencies: vec![],
            order: 0,
        })
    }
    async fn delete_task(&self, _task_id: &str, _jwt: Option<&str>) -> anyhow::Result<()> {
        self.record("delete_task");
        Ok(())
    }
    async fn transition_task(
        &self,
        task_id: &str,
        status: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        self.record("transition_task");
        Ok(TaskDescriptor {
            id: task_id.into(),
            spec_id: String::new(),
            project_id: String::new(),
            title: String::new(),
            description: String::new(),
            status: status.into(),
            dependencies: vec![],
            order: 0,
        })
    }
    async fn claim_next_task(
        &self,
        _project_id: &str,
        _agent_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        self.record("claim_next_task");
        Ok(None)
    }
    async fn get_task(&self, task_id: &str, _jwt: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        self.record("get_task");
        Ok(TaskDescriptor {
            id: task_id.into(),
            spec_id: String::new(),
            project_id: String::new(),
            title: String::new(),
            description: String::new(),
            status: "open".into(),
            dependencies: vec![],
            order: 0,
        })
    }
    async fn get_project(
        &self,
        project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        self.record("get_project");
        Ok(ProjectDescriptor {
            id: project_id.into(),
            name: "p".into(),
            path: String::new(),
            description: None,
            tech_stack: None,
            build_command: None,
            test_command: None,
        })
    }
    async fn update_project(
        &self,
        project_id: &str,
        _updates: ProjectUpdate,
        _jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        self.record("update_project");
        Ok(ProjectDescriptor {
            id: project_id.into(),
            name: "p".into(),
            path: String::new(),
            description: None,
            tech_stack: None,
            build_command: None,
            test_command: None,
        })
    }
    async fn create_log(
        &self,
        _project_id: &str,
        _message: &str,
        _level: &str,
        _agent_id: Option<&str>,
        _metadata: Option<&serde_json::Value>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.record("create_log");
        Ok(json!({ "ok": true }))
    }
    async fn list_logs(
        &self,
        _project_id: &str,
        _level: Option<&str>,
        _limit: Option<u64>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.record("list_logs");
        Ok(json!([]))
    }
    async fn get_project_stats(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        self.record("get_project_stats");
        Ok(json!({}))
    }
    async fn list_messages(
        &self,
        _project_id: &str,
        _instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>> {
        self.record("list_messages");
        Ok(vec![])
    }
    async fn save_message(&self, _params: SaveMessageParams) -> anyhow::Result<()> {
        self.record("save_message");
        Ok(())
    }
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor> {
        self.record("create_session");
        Ok(SessionDescriptor {
            id: "s1".into(),
            instance_id: params.instance_id,
            project_id: params.project_id,
            status: "active".into(),
        })
    }
    async fn get_active_session(
        &self,
        _instance_id: &str,
    ) -> anyhow::Result<Option<SessionDescriptor>> {
        self.record("get_active_session");
        Ok(None)
    }
    async fn orbit_api_call(
        &self,
        method: &str,
        _path: &str,
        _body: Option<&serde_json::Value>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        self.record("orbit_api_call");
        Ok(format!("orbit:{method}"))
    }
    async fn network_api_call(
        &self,
        method: &str,
        _path: &str,
        _body: Option<&serde_json::Value>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        self.record("network_api_call");
        Ok(format!("network:{method}"))
    }
}

fn build_kernel() -> (Arc<Kernel>, Arc<dyn Store>, TempDir, TempDir) {
    let db = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db.path(), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));
    let cfg = KernelConfig {
        workspace_base: ws.path().to_path_buf(),
        ..KernelConfig::default()
    };
    let kernel = Arc::new(
        Kernel::new(
            store.clone(),
            provider,
            ExecutorRouter::new(),
            cfg,
            AgentId::generate(),
        )
        .unwrap(),
    );
    (kernel, store, db, ws)
}

fn count_domain_mutation_entries(store: &Arc<dyn Store>, kernel: &Kernel) -> Vec<Value> {
    let entries = store.scan_record(kernel.agent_id, 0, 256).unwrap();
    entries
        .into_iter()
        .filter(|e| e.tx.tx_type == TransactionType::System)
        .filter_map(|e| serde_json::from_slice::<Value>(&e.tx.payload).ok())
        .filter(|p| p.get("system_kind").and_then(Value::as_str) == Some("domain_mutation"))
        .collect()
}

#[tokio::test]
async fn readonly_methods_passthrough_without_recording() {
    let (kernel, store, _db, _ws) = build_kernel();
    let inner = Arc::new(MockDomain::new());
    let gw = KernelDomainGateway::new(inner.clone(), kernel.clone());

    let _ = gw.list_tasks("p1", None, None).await.unwrap();
    let _ = gw.get_project("p1", None).await.unwrap();
    let _ = gw.list_specs("p1", None).await.unwrap();
    let _ = gw.get_spec("s1", None).await.unwrap();

    assert_eq!(inner.list_tasks_calls.load(Ordering::SeqCst), 1);

    let entries = count_domain_mutation_entries(&store, &kernel);
    assert!(
        entries.is_empty(),
        "read-only methods must not record DomainMutation entries, got: {entries:?}"
    );
}

#[tokio::test]
async fn mutating_method_records_request_and_response_entries() {
    let (kernel, store, _db, _ws) = build_kernel();
    let inner = Arc::new(MockDomain::new());
    let gw = KernelDomainGateway::new(inner.clone(), kernel.clone());

    let spec = gw
        .create_spec("proj-42", "Title", "Body", 0, None)
        .await
        .expect("create_spec succeeds");
    assert_eq!(spec.id, "new-spec");

    let entries = count_domain_mutation_entries(&store, &kernel);
    assert_eq!(
        entries.len(),
        2,
        "expected request+response entries, got {entries:?}"
    );
    let phases: Vec<&str> = entries
        .iter()
        .filter_map(|p| p.get("phase").and_then(Value::as_str))
        .collect();
    assert_eq!(phases, vec!["request", "response"]);
    assert_eq!(entries[1].get("status").and_then(Value::as_str), Some("ok"));
    assert_eq!(
        entries[0].get("method").and_then(Value::as_str),
        Some("create_spec")
    );
}

#[tokio::test]
async fn mutating_method_records_on_failure() {
    let (kernel, store, _db, _ws) = build_kernel();
    let inner = Arc::new(MockDomain::with_failing_create_spec());
    let gw = KernelDomainGateway::new(inner.clone(), kernel.clone());

    let err = gw
        .create_spec("proj-42", "Title", "Body", 0, None)
        .await
        .expect_err("create_spec must propagate domain failure");
    assert!(err.to_string().contains("simulated domain failure"));

    let entries = count_domain_mutation_entries(&store, &kernel);
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[1].get("status").and_then(Value::as_str),
        Some("error"),
        "second entry must carry status=error, got: {:?}",
        entries[1]
    );
    assert!(entries[1]
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .contains("simulated domain failure"));
}

#[tokio::test]
async fn orbit_get_is_passthrough_post_is_recorded() {
    let (kernel, store, _db, _ws) = build_kernel();
    let inner = Arc::new(MockDomain::new());
    let gw = KernelDomainGateway::new(inner.clone(), kernel.clone());

    let _ = gw
        .orbit_api_call("GET", "/repos", None, None)
        .await
        .unwrap();
    let no_entries = count_domain_mutation_entries(&store, &kernel);
    assert!(no_entries.is_empty(), "GET must not record");

    let _ = gw
        .orbit_api_call("POST", "/repos", None, None)
        .await
        .unwrap();
    let entries = count_domain_mutation_entries(&store, &kernel);
    assert_eq!(entries.len(), 2);
}
