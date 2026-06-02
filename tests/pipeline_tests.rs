//! Pipeline integration tests.
//!
//! Tests the full pipeline: transaction submission → agent processing → record entry creation.
//! Also includes determinism and concurrency tests.
//!
//! These tests drive the modern [`KernelToolGateway`] as the [`AgentLoop`]'s
//! tool executor, and they route record appends through [`Kernel::process_dequeued`]
//! / [`Kernel::process_direct`] so the sealed `WriteStore` family
//! (`append_entry_atomic` / `append_entry_direct` / `append_entries_batch`)
//! stays confined to `aura-kernel` per Invariant §10.

use aura_agent::{AgentLoop, AgentLoopConfig, KernelToolGateway};
use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_core_types::{AgentId, Transaction, TransactionType};
use aura_model_reasoner::{MockProvider, MockResponse, ModelProvider, ToolDefinition};
use aura_store_db::{RocksStore, Store};
use bytes::Bytes;
use std::sync::Arc;
use tempfile::TempDir;

/// Create a test environment with a shared store + workspace root.
fn create_pipeline_env() -> (Arc<dyn Store>, TempDir, TempDir) {
    let db_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    (store, db_dir, ws_dir)
}

/// Build a kernel bound to `agent_id` with an empty executor router and the
/// given provider. Used everywhere a test needs to materialise record entries
/// through the canonical kernel write path.
fn build_kernel(
    store: Arc<dyn Store>,
    ws_dir: &TempDir,
    agent_id: AgentId,
    provider: Arc<dyn ModelProvider + Send + Sync>,
) -> Arc<Kernel> {
    let router = ExecutorRouter::new();
    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        ..KernelConfig::default()
    };
    Arc::new(Kernel::new(store, provider, router, config, agent_id).unwrap())
}

// ============================================================================
// Pipeline Tests
// ============================================================================

#[tokio::test]
async fn test_full_pipeline_enqueue_process_record() {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("I completed the task."));
    let (store, _db_dir, ws_dir) = create_pipeline_env();
    let agent_id = AgentId::generate();

    let tx = Transaction::new_chained(
        agent_id,
        TransactionType::UserPrompt,
        Bytes::from("Hello agent"),
        None,
    );
    store.enqueue_tx(&tx).unwrap();
    assert!(store.has_pending_tx(agent_id).unwrap());

    let (token, dequeued_tx) = store.dequeue_tx(agent_id).unwrap().unwrap();
    assert_eq!(dequeued_tx.hash, tx.hash);

    let kernel = build_kernel(store.clone(), &ws_dir, agent_id, provider.clone());
    let executor = KernelToolGateway::new(kernel.clone());

    let config = AgentLoopConfig::for_agent("claude-test-model");
    let agent_loop = AgentLoop::new(config);

    let prompt = String::from_utf8(dequeued_tx.payload.to_vec()).unwrap();
    let messages = vec![aura_model_reasoner::Message::user(prompt)];
    let result = agent_loop
        .run(provider.as_ref(), &executor, messages, vec![])
        .await
        .unwrap();

    assert_eq!(result.iterations, 1);
    assert!(result.total_text.contains("completed the task"));

    // Finalise the user-prompt transaction through the kernel so the record
    // append lands atomically with the inbox dequeue (Invariant §10).
    kernel.process_dequeued(dequeued_tx, token).await.unwrap();

    assert_eq!(store.get_head_seq(agent_id).unwrap(), 1);
    assert!(!store.has_pending_tx(agent_id).unwrap());

    let entries = store.scan_record(agent_id, 1, 10).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].seq, 1);
}

#[tokio::test]
async fn test_pipeline_multiple_transactions() {
    let provider: Arc<dyn ModelProvider + Send + Sync> = Arc::new(
        MockProvider::new()
            .with_response(MockResponse::text("Response 1"))
            .with_response(MockResponse::text("Response 2"))
            .with_response(MockResponse::text("Response 3")),
    );
    let (store, _db_dir, ws_dir) = create_pipeline_env();
    let agent_id = AgentId::generate();

    for i in 1..=3 {
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from(format!("Message {i}")),
            None,
        );
        store.enqueue_tx(&tx).unwrap();
    }

    let kernel = build_kernel(store.clone(), &ws_dir, agent_id, provider.clone());
    let executor = KernelToolGateway::new(kernel.clone());

    let config = AgentLoopConfig::for_agent("claude-test-model");
    let agent_loop = AgentLoop::new(config);

    let mut processed = 0u64;
    while let Some((token, tx)) = store.dequeue_tx(agent_id).unwrap() {
        let prompt = String::from_utf8(tx.payload.to_vec()).unwrap();
        let messages = vec![aura_model_reasoner::Message::user(prompt)];
        let _result = agent_loop
            .run(provider.as_ref(), &executor, messages, vec![])
            .await
            .unwrap();

        kernel.process_dequeued(tx, token).await.unwrap();
        processed += 1;
    }

    assert_eq!(processed, 3);
    assert_eq!(store.get_head_seq(agent_id).unwrap(), 3);

    let entries = store.scan_record(agent_id, 1, 10).unwrap();
    assert_eq!(entries.len(), 3);
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, (i + 1) as u64);
    }
}

// ============================================================================
// Determinism Tests
// ============================================================================

#[tokio::test]
async fn test_deterministic_processing_same_input() {
    let make_provider = || -> Arc<dyn ModelProvider + Send + Sync> {
        Arc::new(MockProvider::simple_response("Deterministic response."))
    };

    let mut results = Vec::new();

    for _ in 0..2 {
        let provider = make_provider();
        let (store, _db_dir, ws_dir) = create_pipeline_env();
        let agent_id = AgentId::generate();

        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from("determinism test"),
            None,
        );
        store.enqueue_tx(&tx).unwrap();

        let kernel = build_kernel(store.clone(), &ws_dir, agent_id, provider.clone());
        let executor = KernelToolGateway::new(kernel);

        let config = AgentLoopConfig::for_agent("claude-test-model");
        let agent_loop = AgentLoop::new(config);

        let messages = vec![aura_model_reasoner::Message::user("determinism test")];
        let result = agent_loop
            .run(provider.as_ref(), &executor, messages, vec![])
            .await
            .unwrap();
        results.push(result);
    }

    assert_eq!(results[0].iterations, results[1].iterations);
    assert_eq!(results[0].total_text, results[1].total_text);
    assert_eq!(results[0].total_input_tokens, results[1].total_input_tokens);
    assert_eq!(
        results[0].total_output_tokens,
        results[1].total_output_tokens
    );
}

#[tokio::test]
async fn test_deterministic_record_entry_seq() {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("ok"));
    let (store, _db_dir, ws_dir) = create_pipeline_env();
    let agent_id = AgentId::generate();
    let kernel = build_kernel(store.clone(), &ws_dir, agent_id, provider);

    for i in 1..=5 {
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from(format!("msg {i}")),
            None,
        );
        store.enqueue_tx(&tx).unwrap();

        let (token, dequeued) = store.dequeue_tx(agent_id).unwrap().unwrap();
        kernel.process_dequeued(dequeued, token).await.unwrap();
    }

    let entries = store.scan_record(agent_id, 1, 10).unwrap();
    assert_eq!(entries.len(), 5);
    for (idx, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, (idx + 1) as u64);
    }
}

// ============================================================================
// Multi-Agent Concurrent Processing Tests
// ============================================================================

#[tokio::test]
async fn test_multi_agent_concurrent_processing() {
    let db_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());

    let agent_ids: Vec<AgentId> = (0..3).map(|_| AgentId::generate()).collect();

    for agent_id in &agent_ids {
        let tx = Transaction::new_chained(
            *agent_id,
            TransactionType::UserPrompt,
            Bytes::from("concurrent test"),
            None,
        );
        store.enqueue_tx(&tx).unwrap();
    }

    let mut handles = Vec::new();
    for agent_id in agent_ids.clone() {
        let store = store.clone();
        let ws_base = ws_dir.path().to_path_buf();

        let handle = tokio::spawn(async move {
            let provider: Arc<dyn ModelProvider + Send + Sync> =
                Arc::new(MockProvider::simple_response("concurrent response"));
            let router = ExecutorRouter::new();
            let config = KernelConfig {
                workspace_base: ws_base,
                ..KernelConfig::default()
            };
            let kernel = Arc::new(
                Kernel::new(store.clone(), provider.clone(), router, config, agent_id).unwrap(),
            );
            let executor = KernelToolGateway::new(kernel.clone());

            let agent_loop = AgentLoop::new(AgentLoopConfig::for_agent("claude-test-model"));

            let (token, tx) = store.dequeue_tx(agent_id).unwrap().unwrap();
            let prompt = String::from_utf8(tx.payload.to_vec()).unwrap();
            let messages = vec![aura_model_reasoner::Message::user(prompt)];
            let _result = agent_loop
                .run(provider.as_ref(), &executor, messages, vec![])
                .await
                .unwrap();

            kernel.process_dequeued(tx, token).await.unwrap();

            agent_id
        });
        handles.push(handle);
    }

    let results: Vec<AgentId> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(results.len(), 3);

    for agent_id in &agent_ids {
        assert_eq!(store.get_head_seq(*agent_id).unwrap(), 1);
        let entries = store.scan_record(*agent_id, 1, 10).unwrap();
        assert_eq!(entries.len(), 1);
    }
}

#[tokio::test]
async fn test_agents_independent_state() {
    let db_dir = TempDir::new().unwrap();
    let ws_dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());

    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("ok"));

    let agent_a = AgentId::generate();
    let agent_b = AgentId::generate();
    let kernel_a = build_kernel(store.clone(), &ws_dir, agent_a, provider.clone());
    let kernel_b = build_kernel(store.clone(), &ws_dir, agent_b, provider);

    for i in 1..=3 {
        let tx = Transaction::new_chained(
            agent_a,
            TransactionType::UserPrompt,
            Bytes::from(format!("agent_a msg {i}")),
            None,
        );
        store.enqueue_tx(&tx).unwrap();
    }

    let tx_b = Transaction::new_chained(
        agent_b,
        TransactionType::UserPrompt,
        Bytes::from("agent_b single msg"),
        None,
    );
    store.enqueue_tx(&tx_b).unwrap();

    for _ in 1..=3 {
        let (token, tx) = store.dequeue_tx(agent_a).unwrap().unwrap();
        kernel_a.process_dequeued(tx, token).await.unwrap();
    }

    let (token, tx) = store.dequeue_tx(agent_b).unwrap().unwrap();
    kernel_b.process_dequeued(tx, token).await.unwrap();

    assert_eq!(store.get_head_seq(agent_a).unwrap(), 3);
    assert_eq!(store.get_head_seq(agent_b).unwrap(), 1);
    assert_eq!(store.scan_record(agent_a, 1, 10).unwrap().len(), 3);
    assert_eq!(store.scan_record(agent_b, 1, 10).unwrap().len(), 1);
}

// ============================================================================
// Tool Use Pipeline Tests
// ============================================================================

#[tokio::test]
async fn test_pipeline_with_tool_use() {
    let provider: Arc<dyn ModelProvider + Send + Sync> = Arc::new(
        MockProvider::new()
            .with_response(MockResponse::tool_use(
                "t1",
                "read_file",
                serde_json::json!({"path": "test.txt"}),
            ))
            .with_response(MockResponse::text("Read complete.")),
    );
    let (store, _db_dir, ws_dir) = create_pipeline_env();
    let agent_id = AgentId::generate();

    let ws_path = ws_dir.path().join(agent_id.to_hex());
    std::fs::create_dir_all(&ws_path).unwrap();
    std::fs::write(ws_path.join("test.txt"), "file contents").unwrap();

    let tx = Transaction::new_chained(
        agent_id,
        TransactionType::UserPrompt,
        Bytes::from("Read test.txt"),
        None,
    );
    store.enqueue_tx(&tx).unwrap();

    let (token, dequeued) = store.dequeue_tx(agent_id).unwrap().unwrap();

    let mut router = ExecutorRouter::new();
    router.add_executor(Arc::new(aura_tools::ToolExecutor::with_defaults()));
    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        ..KernelConfig::default()
    };
    let kernel =
        Arc::new(Kernel::new(store.clone(), provider.clone(), router, config, agent_id).unwrap());
    let executor = KernelToolGateway::new(kernel.clone());

    let agent_loop = AgentLoop::new(AgentLoopConfig::for_agent("claude-test-model"));

    let tools = vec![ToolDefinition::new(
        "read_file",
        "Read a file",
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
    )];

    let prompt = String::from_utf8(dequeued.payload.to_vec()).unwrap();
    let messages = vec![aura_model_reasoner::Message::user(prompt)];
    let result = agent_loop
        .run(provider.as_ref(), &executor, messages, tools)
        .await
        .unwrap();

    assert_eq!(result.iterations, 2);
    assert!(result.total_text.contains("Read complete"));

    // Kernel::process_tools already appended a ToolProposal entry as part of
    // the gateway roundtrip; finalise the UserPrompt entry now.
    kernel.process_dequeued(dequeued, token).await.unwrap();

    // head_seq should be at least 1 (UserPrompt) — tool proposals may have
    // landed additional entries in between.
    assert!(store.get_head_seq(agent_id).unwrap() >= 1);
}

#[tokio::test]
async fn test_store_inbox_depth() {
    let db_dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    let agent_id = AgentId::generate();

    assert_eq!(store.get_inbox_depth(agent_id).unwrap(), 0);

    for i in 0..5 {
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from(format!("msg {i}")),
            None,
        );
        store.enqueue_tx(&tx).unwrap();
    }

    assert_eq!(store.get_inbox_depth(agent_id).unwrap(), 5);
}

#[tokio::test]
async fn test_empty_inbox_dequeue_returns_none() {
    let db_dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
    let agent_id = AgentId::generate();

    assert!(store.dequeue_tx(agent_id).unwrap().is_none());
}
