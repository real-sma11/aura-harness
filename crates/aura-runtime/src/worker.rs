//! Worker for processing agent transactions via kernel-mediated `AgentLoop`.

use aura_agent::{AgentLoop, AgentLoopResult, KernelModelGateway, KernelToolGateway};
use aura_core::AgentId;
use aura_kernel::Kernel;
use aura_reasoner::{Message, ToolDefinition};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};

const AGENT_LOOP_TIMEOUT: Duration = Duration::from_secs(300);

/// Summary returned after draining an agent inbox.
#[derive(Debug, Default)]
pub struct ProcessedAgent {
    pub processed: u64,
    pub last_result: Option<AgentLoopResult>,
}

/// Detailed variant used by foreground subagent dispatch to retrieve the
/// final child result while preserving the normal worker path for callers that
/// only need a processed-count.
///
/// Span name `worker` keeps the chain compact: the parent
/// `agent{id=…}` span (installed by
/// [`crate::scheduler::Scheduler::schedule_agent_with_overrides`])
/// already carries the agent id, so this span adds only the structural
/// hop without re-emitting the id field.
#[instrument(name = "worker", skip_all)]
pub async fn process_agent_detailed(
    agent_id: AgentId,
    kernel: Arc<Kernel>,
    agent_loop: &AgentLoop,
    tools: &[ToolDefinition],
) -> anyhow::Result<ProcessedAgent> {
    let mut processed = 0u64;
    let mut last_result = None;
    let store = kernel.store().clone();

    let model_gateway = KernelModelGateway::new(kernel.clone());
    let tool_gateway = KernelToolGateway::new(kernel.clone());

    loop {
        let Some((token, tx)) = store.dequeue_tx(agent_id)? else {
            debug!(processed, "Inbox empty, worker done");
            break;
        };

        debug!(
            inbox_seq = token.inbox_seq(),
            hash = %tx.hash,
            "Processing transaction"
        );

        let _prompt_result = kernel
            .process_dequeued(tx.clone(), token)
            .await
            .map_err(|e| anyhow::anyhow!("kernel process_dequeued failed: {e}"))?;

        let prompt = String::from_utf8(tx.payload.to_vec())
            .map_err(|e| anyhow::anyhow!("Transaction payload is not valid UTF-8: {e}"))?;
        let messages = vec![Message::user(prompt)];

        let result = tokio::time::timeout(
            AGENT_LOOP_TIMEOUT,
            agent_loop.run(&model_gateway, &tool_gateway, messages, tools.to_vec()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Agent loop timed out after {AGENT_LOOP_TIMEOUT:?}"))??;

        let response_tx = aura_core::Transaction::new_chained(
            agent_id,
            aura_core::TransactionType::AgentMsg,
            result.total_text.as_bytes().to_vec(),
            None,
        );
        let _response_result = kernel
            .process_direct(response_tx)
            .await
            .map_err(|e| anyhow::anyhow!("kernel process_direct (response) failed: {e}"))?;

        if result.llm_error.is_some() {
            warn!("Transaction processed with LLM error");
        } else {
            info!(
                iterations = result.iterations,
                "Transaction committed via kernel-mediated AgentLoop"
            );
        }

        processed += 1;
        last_result = Some(result);
    }

    Ok(ProcessedAgent {
        processed,
        last_result,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{Transaction, TransactionType};
    use aura_kernel::{ExecutorRouter, KernelConfig};
    use aura_reasoner::MockProvider;
    use aura_store::{RocksStore, Store};
    use bytes::Bytes;

    fn create_test_kernel(dir: &std::path::Path, agent_id: AgentId) -> Arc<Kernel> {
        let store: Arc<dyn Store> = Arc::new(RocksStore::open(dir.join("db"), false).unwrap());
        let provider: Arc<dyn aura_reasoner::ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("response"));
        let ws_dir = dir.join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let executor = ExecutorRouter::new();
        let config = KernelConfig {
            workspace_base: ws_dir,
            ..KernelConfig::default()
        };
        Arc::new(Kernel::new(store, provider, executor, config, agent_id).unwrap())
    }

    #[tokio::test]
    async fn test_process_agent_empty_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let agent_id = AgentId::generate();
        let kernel = create_test_kernel(dir.path(), agent_id);
        let agent_loop = AgentLoop::new(aura_agent::AgentLoopConfig::for_agent("claude-opus-4-7"));

        let count = process_agent_detailed(agent_id, kernel, &agent_loop, &[])
            .await
            .unwrap()
            .processed;

        assert_eq!(count, 0, "Empty inbox should process 0 transactions");
    }

    #[tokio::test]
    async fn test_process_agent_single_tx() {
        let dir = tempfile::tempdir().unwrap();
        let agent_id = AgentId::generate();
        let store: Arc<dyn Store> =
            Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let provider: Arc<dyn aura_reasoner::ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("I processed your request."));
        let ws_dir = dir.path().join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();

        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from("test prompt"),
            None,
        );
        store.enqueue_tx(&tx).unwrap();

        let executor = ExecutorRouter::new();
        let config = KernelConfig {
            workspace_base: ws_dir,
            ..KernelConfig::default()
        };
        let kernel =
            Arc::new(Kernel::new(store.clone(), provider, executor, config, agent_id).unwrap());
        let agent_loop = AgentLoop::new(aura_agent::AgentLoopConfig::for_agent("claude-opus-4-7"));

        let count = process_agent_detailed(agent_id, kernel, &agent_loop, &[])
            .await
            .unwrap()
            .processed;

        assert_eq!(count, 1, "Should process exactly 1 transaction");
        assert!(
            store.get_head_seq(agent_id).unwrap() >= 2,
            "Kernel records prompt + response, so head_seq >= 2"
        );
    }

    #[test]
    fn test_agent_loop_timeout_constant() {
        assert_eq!(AGENT_LOOP_TIMEOUT, Duration::from_secs(300));
    }
}
