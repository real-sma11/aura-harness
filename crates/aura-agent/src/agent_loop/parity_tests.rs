//! Parity tests verifying parallel execution, timeouts, and policy enforcement
//! exercised through the [`KernelToolGateway`] ↔ [`AgentLoop`] stack.
//!
//! These tests used to drive the deprecated `KernelToolExecutor` directly so
//! they could twiddle per-executor knobs (`.with_parallel()`, `.with_timeout(...)`,
//! `.with_policy(...)`). Wave 2 T5 re-routes them through the kernel so the
//! properties they cover — parallel batch execution, per-tool timeout, and
//! policy denial — are validated on the real gateway surface.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use aura_agent_kernel::{
    ExecuteContext, Executor, ExecutorError, ExecutorRouter, Kernel, KernelConfig, PolicyConfig,
};
use aura_core_types::{
    Action, ActionKind, AgentId, AgentToolPermissions, Effect, ToolCall, ToolResult, ToolState,
};
use aura_model_reasoner::{
    ContentBlock, Message, MockProvider, MockResponse, ModelProvider, StopReason, ToolDefinition,
    Usage,
};
use aura_store_db::{RocksStore, Store};

use crate::agent_loop::{AgentLoop, AgentLoopConfig};
use crate::kernel_gateway::KernelToolGateway;
use crate::types::{AgentToolExecutor, ToolCallInfo};

// ---------------------------------------------------------------------------
// Stub kernel-level executors
// ---------------------------------------------------------------------------

/// Returns a canned `ToolResult::success` for every delegate action.
struct StubExecutor;

#[async_trait]
impl Executor for StubExecutor {
    async fn execute(
        &self,
        _ctx: &ExecuteContext,
        action: &Action,
    ) -> Result<Effect, ExecutorError> {
        let tool_call: ToolCall = serde_json::from_slice(&action.payload)
            .map_err(|e| ExecutorError::ExecutionFailed(e.to_string()))?;
        let result = ToolResult::success(&tool_call.tool, format!("ok:{}", tool_call.tool));
        let payload = serde_json::to_vec(&result)
            .map_err(|e| ExecutorError::ExecutionFailed(e.to_string()))?;
        Ok(Effect::committed_agreement(action.action_id, payload))
    }

    fn can_handle(&self, action: &Action) -> bool {
        action.kind == ActionKind::Delegate
    }

    fn name(&self) -> &'static str {
        "stub"
    }
}

/// Sleeps for a configurable duration before returning, used to trigger timeouts.
struct SlowExecutor {
    delay: Duration,
}

#[async_trait]
impl Executor for SlowExecutor {
    async fn execute(
        &self,
        _ctx: &ExecuteContext,
        action: &Action,
    ) -> Result<Effect, ExecutorError> {
        tokio::time::sleep(self.delay).await;
        let tool_call: ToolCall = serde_json::from_slice(&action.payload)
            .map_err(|e| ExecutorError::ExecutionFailed(e.to_string()))?;
        let result = ToolResult::success(&tool_call.tool, "slow-done");
        let payload = serde_json::to_vec(&result)
            .map_err(|e| ExecutorError::ExecutionFailed(e.to_string()))?;
        Ok(Effect::committed_agreement(action.action_id, payload))
    }

    fn can_handle(&self, action: &Action) -> bool {
        action.kind == ActionKind::Delegate
    }

    fn name(&self) -> &'static str {
        "slow"
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct KernelHarness {
    kernel: Arc<Kernel>,
    _db_dir: tempfile::TempDir,
    _ws_dir: tempfile::TempDir,
}

fn build_kernel(router: ExecutorRouter, config: KernelConfig) -> KernelHarness {
    let db_dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("parity-test"));
    let mut config = config;
    config.workspace_base = ws_dir.path().to_path_buf();
    let kernel =
        Arc::new(Kernel::new(store, provider, router, config, AgentId::generate()).unwrap());
    KernelHarness {
        kernel,
        _db_dir: db_dir,
        _ws_dir: ws_dir,
    }
}

fn stub_router() -> ExecutorRouter {
    ExecutorRouter::with_executors(vec![Arc::new(StubExecutor)])
}

fn make_tool_call_info(id: &str, name: &str) -> ToolCallInfo {
    ToolCallInfo {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({}),
    }
}

fn two_tool_use_response() -> MockResponse {
    MockResponse {
        stop_reason: StopReason::ToolUse,
        content: vec![
            ContentBlock::tool_use("t1", "read_file", serde_json::json!({"path": "a.txt"})),
            ContentBlock::tool_use("t2", "read_file", serde_json::json!({"path": "b.txt"})),
        ],
        usage: Usage::new(100, 50),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn parallel_read_tools_execute_concurrently() {
    // Kernel `process_tools` runs every proposal in a batch via `join_all`,
    // so the parallel behaviour previously opted into via
    // `KernelToolExecutor::with_parallel` is now the default path.
    let harness = build_kernel(stub_router(), KernelConfig::default());
    let executor = KernelToolGateway::new(harness.kernel);

    let provider = MockProvider::new()
        .with_response(two_tool_use_response())
        .with_response(MockResponse::text("done"));

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("go")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2, "should run tool-use + final turn");
    assert!(result.total_text.contains("done"));
}

#[tokio::test]
async fn parallel_tools_preserve_result_order() {
    let harness = build_kernel(stub_router(), KernelConfig::default());
    let executor = KernelToolGateway::new(harness.kernel);

    let calls = vec![
        make_tool_call_info("t1", "read_file"),
        make_tool_call_info("t2", "list_files"),
    ];

    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].tool_use_id, "t1");
    assert_eq!(results[1].tool_use_id, "t2");
    assert!(!results[0].is_error);
    assert!(!results[1].is_error);
    assert!(results[0].content.contains("read_file"));
    assert!(results[1].content.contains("list_files"));
}

#[tokio::test]
async fn tool_timeout_returns_error() {
    let slow_router = ExecutorRouter::with_executors(vec![Arc::new(SlowExecutor {
        delay: Duration::from_secs(5),
    })]);
    // `KernelConfig::tool_timeout_ms` replaces the old
    // `KernelToolExecutor::with_timeout` knob.
    let harness = build_kernel(
        slow_router,
        KernelConfig {
            tool_timeout_ms: 50,
            ..KernelConfig::default()
        },
    );
    let executor = KernelToolGateway::new(harness.kernel);

    let calls = vec![make_tool_call_info("t1", "read_file")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains("timed out"),
        "expected timeout message, got: {}",
        results[0].content,
    );
}

#[tokio::test]
async fn policy_deny_returns_error_result() {
    let policy_config = PolicyConfig::default().with_agent_override(Some(
        AgentToolPermissions::new().with("delete_file", ToolState::Deny),
    ));
    let harness = build_kernel(
        stub_router(),
        KernelConfig {
            policy: policy_config,
            ..KernelConfig::default()
        },
    );
    let executor = KernelToolGateway::new(harness.kernel);

    let calls = vec![make_tool_call_info("t1", "delete_file")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains("not allowed"),
        "expected policy denial, got: {}",
        results[0].content,
    );
}

/// Layer E.3: drive a full `AgentLoop::run` through the streaming
/// sampling pump (the production default since Phase 7) so we get
/// end-to-end coverage of the pump → sampling-driver → turn loop →
/// task shell stack. The fake provider scripts a `tool_use`
/// response followed by a `text` finish; the kernel-backed executor
/// returns `ok:read_file` for each call. We assert that the loop
/// terminates, records the right number of iterations, and emits
/// the synthetic final message — the contract previously shared
/// with the legacy `call_model` path before the buffered transport
/// was retired.
#[tokio::test]
async fn stream_pump_path_completes_two_iteration_run() {
    let harness = build_kernel(stub_router(), KernelConfig::default());
    let executor = KernelToolGateway::new(harness.kernel);

    let provider = MockProvider::new()
        .with_response(two_tool_use_response())
        .with_response(MockResponse::text("streamed-done"));

    let config = AgentLoopConfig {
        system_prompt: "test".to_string(),
        ..AgentLoopConfig::for_agent("claude-test-model")
    };
    let agent = AgentLoop::new(config);
    let messages = vec![Message::user("go")];
    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object"}),
    )];

    let result = agent
        .run(&provider, &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(
        result.iterations, 2,
        "pump path should drive the same iteration count as the legacy path"
    );
    assert!(
        result.total_text.contains("streamed-done"),
        "pump path must accumulate the final assistant text"
    );
}

#[tokio::test]
async fn policy_deny_does_not_block_allowed_tools() {
    let policy_config = PolicyConfig::default().with_agent_override(Some(
        AgentToolPermissions::new().with("delete_file", ToolState::Deny),
    ));
    let harness = build_kernel(
        stub_router(),
        KernelConfig {
            policy: policy_config,
            ..KernelConfig::default()
        },
    );
    let executor = KernelToolGateway::new(harness.kernel);

    let calls = vec![make_tool_call_info("t1", "read_file")];
    let results = executor.execute(&calls).await;

    assert_eq!(results.len(), 1);
    assert!(
        !results[0].is_error,
        "read_file should be allowed, got error: {}",
        results[0].content,
    );
    assert!(results[0].content.contains("read_file"));
}
