//! Invariant §3 enforcement for automaton LLM calls.
//!
//! These tests pin the requirement that any automaton that issues a
//! `ModelProvider::complete`-style call MUST go through
//! [`aura_agent::KernelModelGateway`] so the call produces a
//! [`aura_core::TransactionType::Reasoning`] record entry. A failure
//! here indicates that a new code path bypasses the kernel's recording
//! seam — a §1 / §3 regression.

use std::sync::Arc;

use async_trait::async_trait;
use aura_agent::KernelModelGateway;
use aura_automaton::{Automaton, AutomatonId, AutomatonState, TickContext};
use aura_core::{AgentId, TransactionType};
use aura_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_reasoner::{MockProvider, ModelProvider};
use aura_store::{RocksStore, Store};
use aura_tools::domain_tools::{
    CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
    SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Minimal DomainApi mock — only get_project + list_specs + create_spec +
// delete_spec are exercised.
// ---------------------------------------------------------------------------

struct DummyDomain;

#[async_trait]
impl DomainApi for DummyDomain {
    async fn list_specs(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<SpecDescriptor>> {
        Ok(vec![])
    }
    async fn get_spec(&self, _spec_id: &str, _jwt: Option<&str>) -> anyhow::Result<SpecDescriptor> {
        anyhow::bail!("not used")
    }
    async fn create_spec(
        &self,
        project_id: &str,
        title: &str,
        content: &str,
        order: u32,
        _jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        Ok(SpecDescriptor {
            id: format!("spec-{order}"),
            project_id: project_id.into(),
            title: title.into(),
            content: content.into(),
            order,
            parent_id: None,
        })
    }
    async fn update_spec(
        &self,
        _spec_id: &str,
        _title: Option<&str>,
        _content: Option<&str>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<SpecDescriptor> {
        anyhow::bail!("not used")
    }
    async fn delete_spec(&self, _spec_id: &str, _jwt: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list_tasks(
        &self,
        _project_id: &str,
        _spec_id: Option<&str>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Vec<TaskDescriptor>> {
        Ok(vec![])
    }
    async fn create_task(
        &self,
        _project_id: &str,
        _spec_id: &str,
        _title: &str,
        _description: &str,
        _dependencies: &[String],
        _order: u32,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        anyhow::bail!("not used")
    }
    async fn update_task(
        &self,
        _task_id: &str,
        _updates: TaskUpdate,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        anyhow::bail!("not used")
    }
    async fn delete_task(&self, _task_id: &str, _jwt: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    async fn transition_task(
        &self,
        _task_id: &str,
        _status: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<TaskDescriptor> {
        anyhow::bail!("not used")
    }
    async fn claim_next_task(
        &self,
        _project_id: &str,
        _agent_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<Option<TaskDescriptor>> {
        Ok(None)
    }
    async fn get_task(&self, _task_id: &str, _jwt: Option<&str>) -> anyhow::Result<TaskDescriptor> {
        anyhow::bail!("not used")
    }
    async fn get_project(
        &self,
        project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        Ok(ProjectDescriptor {
            id: project_id.into(),
            name: "proj".into(),
            path: String::new(),
            description: None,
            tech_stack: None,
            build_command: None,
            test_command: None,
        })
    }
    async fn update_project(
        &self,
        _project_id: &str,
        _updates: ProjectUpdate,
        _jwt: Option<&str>,
    ) -> anyhow::Result<ProjectDescriptor> {
        anyhow::bail!("not used")
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
        Ok(serde_json::json!({}))
    }
    async fn list_logs(
        &self,
        _project_id: &str,
        _level: Option<&str>,
        _limit: Option<u64>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!([]))
    }
    async fn get_project_stats(
        &self,
        _project_id: &str,
        _jwt: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    async fn list_messages(
        &self,
        _project_id: &str,
        _instance_id: &str,
    ) -> anyhow::Result<Vec<MessageDescriptor>> {
        Ok(vec![])
    }
    async fn save_message(&self, _params: SaveMessageParams) -> anyhow::Result<()> {
        Ok(())
    }
    async fn create_session(
        &self,
        params: CreateSessionParams,
    ) -> anyhow::Result<SessionDescriptor> {
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
        Ok(None)
    }
    async fn orbit_api_call(
        &self,
        _method: &str,
        _path: &str,
        _body: Option<&serde_json::Value>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn network_api_call(
        &self,
        _method: &str,
        _path: &str,
        _body: Option<&serde_json::Value>,
        _jwt: Option<&str>,
    ) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

/// Mock provider that returns a fixed JSON array valid for SpecGen's
/// parser so the automaton's tick completes a single LLM call and then
/// proceeds to save specs.
fn spec_gen_mock_provider() -> Arc<dyn ModelProvider + Send + Sync> {
    Arc::new(MockProvider::simple_response(
        r#"[{"title": "S1", "markdown_contents": "body"}]"#,
    ))
}

fn build_kernel(
    provider: Arc<dyn ModelProvider + Send + Sync>,
) -> (Arc<Kernel>, Arc<dyn Store>, TempDir, TempDir) {
    let db = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db.path(), false).unwrap());
    let cfg = KernelConfig {
        workspace_base: ws.path().to_path_buf(),
        use_workspace_base_as_root: true,
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

#[tokio::test]
async fn spec_gen_via_kernel_model_gateway_records_reasoning_entry() {
    let raw_provider = spec_gen_mock_provider();
    let (kernel, store, _db, ws) = build_kernel(raw_provider);

    // Write a minimal requirements document inside the workspace.
    let req_path = ws.path().join("requirements.md");
    std::fs::write(&req_path, "A product requirements document.").unwrap();

    // The SpecGen automaton receives a KernelModelGateway, not the raw
    // provider — this is the §1/§3 invariant we're pinning. The
    // `RecordingModelProvider` seal in `aura-agent` enforces this:
    // SpecGenAutomaton::new will only accept an `Arc<P>` where `P`
    // implements the sealed marker trait, and `KernelModelGateway`
    // is the only public implementor.
    let gateway: Arc<KernelModelGateway> = Arc::new(KernelModelGateway::new(kernel.clone()));
    let domain: Arc<dyn DomainApi> = Arc::new(DummyDomain);
    let automaton = aura_automaton::SpecGenAutomaton::new(domain, gateway);

    let (event_tx, _event_rx) = mpsc::channel(64);
    let mut ctx = TickContext::new(
        AutomatonId::new(),
        AutomatonState::new(),
        event_tx,
        serde_json::json!({
            "project_id": "p1",
            "requirements_path": "requirements.md",
            // `SpecGen::generate_specs` now requires an explicit
            // model id — the silent `DEFAULT_MODEL` fallback was
            // removed in `5618cce`. Any model name satisfies the
            // mock provider, which ignores the field.
            "model": "claude-opus-4-7",
        }),
        Some(ws.path().to_path_buf()),
        CancellationToken::new(),
    );

    let outcome = automaton.tick(&mut ctx).await.expect("spec-gen tick");
    assert!(
        matches!(outcome, aura_automaton::TickOutcome::Done),
        "spec-gen should complete in one tick"
    );

    let entries = store.scan_record(kernel.agent_id, 0, 64).unwrap();
    let reasoning: Vec<_> = entries
        .iter()
        .filter(|e| e.tx.tx_type == TransactionType::Reasoning)
        .collect();
    assert!(
        !reasoning.is_empty(),
        "spec-gen must produce at least one Reasoning record entry; found entries: {:?}",
        entries
            .iter()
            .map(|e| format!("{:?}", e.tx.tx_type))
            .collect::<Vec<_>>()
    );
}
