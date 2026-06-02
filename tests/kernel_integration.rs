//! End-to-end integration test for kernel-mediated processing.
//!
//! Verifies the full flow: process → reason → process_tools → reason → process,
//! checking all invariants hold.

use aura_agent_kernel::{ExecutorRouter, Kernel, KernelConfig};
use aura_core_types::{AgentId, TransactionType};
use aura_model_reasoner::{Message, MockProvider, ModelRequest};
use aura_store_db::{RocksStore, Store};
use std::sync::Arc;
use tempfile::TempDir;

fn create_test_kernel(
    provider: Arc<dyn aura_model_reasoner::ModelProvider + Send + Sync>,
) -> (Arc<Kernel>, Arc<dyn Store>, TempDir) {
    let dir = TempDir::new().unwrap();
    let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
    let agent_id = AgentId::generate();
    let ws_dir = dir.path().join("workspaces");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let config = KernelConfig {
        workspace_base: ws_dir,
        ..KernelConfig::default()
    };
    let executor = ExecutorRouter::new();
    let kernel =
        Arc::new(Kernel::new(store.clone(), provider, executor, config, agent_id).unwrap());
    (kernel, store, dir)
}

#[tokio::test]
async fn test_full_kernel_mediated_flow() {
    let provider: Arc<dyn aura_model_reasoner::ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("I'll help with that."));
    let (kernel, store, _dir) = create_test_kernel(provider);

    // 1. Process a UserPrompt
    let tx1 = aura_core_types::Transaction::user_prompt(kernel.agent_id, "Hello agent");
    let r1 = kernel.process_direct(tx1).await.unwrap();
    assert_eq!(r1.entry.seq, 1, "First entry should be seq 1");
    assert!(r1.tool_output.is_none());
    assert!(!r1.had_failures);

    // 2. Reason (LLM call)
    let request = ModelRequest::builder("test-model", "system prompt")
        .message(Message::user("Hello agent"))
        .try_build()
        .unwrap();
    let r2 = kernel.reason(request).await.unwrap();
    assert_eq!(r2.entry.seq, 2, "Reasoning should be seq 2");
    assert_eq!(r2.entry.tx.tx_type, TransactionType::Reasoning);

    // 3. Process an AgentMsg (response)
    let response_tx = aura_core_types::Transaction::new_chained(
        kernel.agent_id,
        TransactionType::AgentMsg,
        "I'll help with that.",
        None,
    );
    let r3 = kernel.process_direct(response_tx).await.unwrap();
    assert_eq!(r3.entry.seq, 3, "Response should be seq 3");

    // 4. Verify monotonic sequencing in store
    let head = store.get_head_seq(kernel.agent_id).unwrap();
    assert_eq!(head, 3, "Head should be at seq 3");

    let entries = store.scan_record(kernel.agent_id, 1, 10).unwrap();
    assert_eq!(entries.len(), 3, "Should have 3 entries");

    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, (i + 1) as u64, "Sequence should be monotonic");
    }

    // Verify context hashes are non-zero and unique
    let hashes: Vec<aura_core_types::ContextHash> =
        entries.iter().map(|e| e.context_hash).collect();
    for hash in &hashes {
        assert_ne!(
            *hash,
            aura_core_types::ContextHash::zero(),
            "Context hash should be non-zero"
        );
    }
    assert_ne!(hashes[0], hashes[1]);
    assert_ne!(hashes[1], hashes[2]);
}

#[tokio::test]
async fn test_dequeued_processing() {
    let provider: Arc<dyn aura_model_reasoner::ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("processed"));
    let (kernel, store, _dir) = create_test_kernel(provider);

    // Enqueue and dequeue
    let tx = aura_core_types::Transaction::user_prompt(kernel.agent_id, "test");
    store.enqueue_tx(&tx).unwrap();
    let (token, dequeued_tx) = store.dequeue_tx(kernel.agent_id).unwrap().unwrap();

    // Process through kernel with dequeue token
    let result = kernel.process_dequeued(dequeued_tx, token).await.unwrap();
    assert_eq!(result.entry.seq, 1);

    // Inbox should be drained
    assert!(!store.has_pending_tx(kernel.agent_id).unwrap());
}

#[tokio::test]
async fn test_sequence_continuity() {
    let provider: Arc<dyn aura_model_reasoner::ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("response"));
    let (kernel, store, _dir) = create_test_kernel(provider);

    // Mix process_direct and reason calls
    for i in 0..5 {
        if i % 2 == 0 {
            let tx = aura_core_types::Transaction::user_prompt(kernel.agent_id, format!("msg {i}"));
            kernel.process_direct(tx).await.unwrap();
        } else {
            let request = ModelRequest::builder("test-model", "system")
                .message(Message::user(format!("msg {i}")))
                .try_build()
                .unwrap();
            kernel.reason(request).await.unwrap();
        }
    }

    let head = store.get_head_seq(kernel.agent_id).unwrap();
    assert_eq!(head, 5);

    let entries = store.scan_record(kernel.agent_id, 1, 10).unwrap();
    assert_eq!(entries.len(), 5);
    for (i, entry) in entries.iter().enumerate() {
        assert_eq!(entry.seq, (i + 1) as u64);
    }
}
