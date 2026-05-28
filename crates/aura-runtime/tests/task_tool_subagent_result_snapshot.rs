//! Phase 7a guard: the JSON shape produced by the `task` tool
//! after routing through the new `aura-fleet-spawn` pipeline must
//! be **byte-identical** to the pre-refactor output.
//!
//! The legacy in-runtime dispatcher and the new fleet-routed
//! dispatcher both serialise an [`aura_core::SubagentResult`] and
//! return it to the parent agent. Any drift in field names,
//! ordering, optionality, or enum tag shape is observable by
//! every existing client (terminal, IDE adapter, external task
//! consumers).
//!
//! ## What this snapshot pins
//!
//! - Top-level `SubagentResult` keys + ordering.
//! - The serde representation of [`aura_core::SubagentExit`]
//!   (internally tagged via `"kind"`, snake_case discriminants).
//! - The `child_agent_id` hex format (redacted to a stable token
//!   here because the value is random per run).
//!
//! ## Failure recipe
//!
//! 1. Inspect the diff in `insta` (UPDATE_SNAPSHOTS=1 cargo test
//!    -p aura-runtime --test task_tool_subagent_result_snapshot).
//! 2. If the change is intentional, ensure every consumer of the
//!    `SubagentResult` wire shape (terminal renderer, IDE adapter,
//!    task tool callers) is updated in the same change.
//! 3. Re-record the snapshot.
//!
//! ## Why this test pre-emptively redacts
//!
//! `child_agent_id` is freshly generated on every run; the
//! `redaction!` macro replaces it with a deterministic
//! `"[child_agent_id]"` token so the snapshot stays stable
//! across runs while still asserting the field is present and
//! the surrounding shape is unchanged.

use std::sync::Arc;

use aura_core::{
    AgentId, AgentPermissions, AgentScope, Capability, SubagentDispatchRequest, SubagentResult,
    UserToolDefaults,
};
use aura_reasoner::MockProvider;
use aura_runtime::scheduler::Scheduler;
use aura_runtime::subagent_dispatch::RuntimeSubagentDispatch;
use aura_store::RocksStore;
use aura_tools::{SubagentDispatchHook, ToolCatalog};

#[tokio::test]
async fn task_tool_subagent_result_json_shape_is_byte_identical() {
    let dir = tempfile::tempdir().expect("temp dir");
    let workspace = tempfile::tempdir().expect("workspace dir");
    let store = Arc::new(RocksStore::open(dir.path().join("db"), false).expect("rocks open"));
    let provider = Arc::new(MockProvider::simple_response("snapshot child output"));
    let catalog = ToolCatalog::default();
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        provider,
        Vec::new(),
        catalog.executor_builtin_tools(),
        workspace.path().to_path_buf(),
        None,
    ));
    let dispatch = RuntimeSubagentDispatch::new(store, scheduler);

    let parent_agent_id = AgentId::generate();
    let result: SubagentResult = dispatch
        .dispatch(SubagentDispatchRequest {
            parent_agent_id,
            subagent_type: "explore".into(),
            prompt: "summarize".into(),
            originating_user_id: Some("snapshot-user".into()),
            parent_chain: Vec::new(),
            model_override: None,
            system_prompt_addendum: None,
            parent_permissions: AgentPermissions {
                scope: AgentScope::default(),
                capabilities: vec![Capability::SpawnAgent],
            },
            parent_tool_permissions: None,
            user_tool_defaults: UserToolDefaults::full_access(),
            tool_call_id: None,
            parent_mode: None,
            parent_kernel_mode: None,
            parent_model_id: None,
            override_mode: None,
            override_permissions: None,
            override_tool_subset: None,
            override_isolation_id: None,
            override_budget: None,
        })
        .await
        .expect("dispatch");

    let json = serde_json::to_value(&result).expect("serialize result");
    insta::assert_json_snapshot!(
        "task_tool_subagent_result_completed",
        json,
        {
            ".child_agent_id" => "[child_agent_id]"
        }
    );
}
