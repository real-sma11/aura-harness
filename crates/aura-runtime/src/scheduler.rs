//! Scheduler for dispatching agent workers.
//!
//! # Concurrency Model
//!
//! The scheduler enforces a **single-writer-per-agent** invariant: at most one
//! task may process a given agent's transaction queue at any time.  Different
//! agents are fully independent and can be processed concurrently.
//!
//! ## Store-Backed Processing Claims
//!
//! Each scheduling attempt claims a persisted per-agent processing marker before
//! constructing a [`Kernel`]. When [`Scheduler::schedule_agent`] is called:
//!
//! 1. A status check is performed (only `Active` agents proceed).
//! 2. A pending-transaction check avoids claiming processing when the inbox is
//!    empty.
//! 3. A store-level compare-and-set claims processing for the agent.
//! 4. All pending transactions are drained without holding an async mutex.
//!
//! Because the claim is per-agent, concurrent calls for *different* agents never
//! block each other. Concurrent calls for the *same* agent are skipped when a
//! claim already exists, preserving caller semantics without head-of-line
//! blocking behind long LLM/tool work.
//!
//! ## Failure Modes
//!
//! * **Panic while claimed** – the local claim guard releases during unwinding.
//!   However, the agent's partially-processed state may be inconsistent; the
//!   store's atomic-batch guarantees prevent *partial record writes*, but the
//!   agent may have committed fewer entries than intended.
//! * **Process crash while claimed** – the persisted claim can remain set after
//!   a hard crash. A future lease timestamp/owner can recover abandoned claims.

use crate::worker::{process_agent_detailed, ProcessedAgent};
use aura_agent::{AgentLoop, AgentLoopConfig};
use aura_core::{AgentId, AgentStatus};
use aura_kernel::{Executor, ExecutorRouter, Kernel, KernelConfig, PolicyConfig};
use aura_reasoner::{ModelProvider, ToolDefinition};
use aura_store::Store;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, error, info, instrument};

/// Local guard that makes store-backed processing claims release on every
/// normal scheduler exit path, with `Drop` as a panic safety net.
pub(crate) struct ProcessingClaim {
    store: Arc<dyn Store>,
    agent_id: AgentId,
    released: bool,
}

impl ProcessingClaim {
    fn try_new(store: Arc<dyn Store>, agent_id: AgentId) -> anyhow::Result<Option<Self>> {
        if !store.try_claim_agent_processing(agent_id)? {
            return Ok(None);
        }
        Ok(Some(Self {
            store,
            agent_id,
            released: false,
        }))
    }

    fn release(&mut self) -> anyhow::Result<()> {
        if !self.released {
            self.store.release_agent_processing(self.agent_id)?;
            self.released = true;
        }
        Ok(())
    }
}

impl Drop for ProcessingClaim {
    fn drop(&mut self) {
        if !self.released {
            if let Err(error) = self.store.release_agent_processing(self.agent_id) {
                error!(agent_id = %self.agent_id, %error, "Failed to release processing claim");
            }
            self.released = true;
        }
    }
}

/// Scheduler for managing agent workers.
pub struct Scheduler {
    // TODO(phase2-followup): Invariant §10 — bind to `Arc<dyn ReadStore>`
    // once `Kernel::new` accepts a `(ReadStore, WriteHook)` pair. The
    // scheduler itself never calls `append_entry_*`; it only forwards the
    // store handle when constructing per-agent kernels.
    store: Arc<dyn Store>,
    provider: Arc<dyn ModelProvider + Send + Sync>,
    agent_loop_config: AgentLoopConfig,
    executors: Vec<Arc<dyn Executor>>,
    tools: Vec<ToolDefinition>,
    kernel_config: KernelConfig,
    memory_manager: Option<Arc<aura_memory::MemoryManager>>,
}

impl Scheduler {
    /// Create a new scheduler.
    #[must_use]
    pub fn new(
        store: Arc<dyn Store>,
        provider: Arc<dyn ModelProvider + Send + Sync>,
        executors: Vec<Arc<dyn Executor>>,
        tools: Vec<ToolDefinition>,
        workspace_base: PathBuf,
        memory_manager: Option<Arc<aura_memory::MemoryManager>>,
    ) -> Self {
        let kernel_config = KernelConfig {
            workspace_base,
            ..KernelConfig::default()
        };
        Self {
            store,
            provider,
            agent_loop_config: AgentLoopConfig::default(),
            executors,
            tools,
            kernel_config,
            memory_manager,
        }
    }

    /// Attempt to claim exclusive processing for a non-scheduler direct append.
    pub(crate) fn try_processing_claim(
        &self,
        agent_id: AgentId,
    ) -> anyhow::Result<Option<ProcessingClaim>> {
        ProcessingClaim::try_new(self.store.clone(), agent_id)
    }

    /// Wait for exclusive processing, used by short direct append paths that
    /// previously awaited the per-agent mutex.
    pub(crate) async fn processing_claim(
        &self,
        agent_id: AgentId,
    ) -> anyhow::Result<ProcessingClaim> {
        loop {
            if let Some(claim) = self.try_processing_claim(agent_id)? {
                return Ok(claim);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Build an `ExecutorRouter` from the shared executor list.
    fn build_executor_router(&self) -> ExecutorRouter {
        ExecutorRouter::with_executors(self.executors.clone())
    }

    /// Schedule processing for an agent.
    ///
    /// Constructs a per-agent [`Kernel`] and routes all transactions through
    /// kernel-mediated processing.
    ///
    /// No `#[instrument]` here: this is a one-line shim that immediately
    /// delegates to [`Self::schedule_agent_with_overrides`], which owns
    /// the canonical `agent{id=...}` span so the prefix chain doesn't
    /// duplicate the `agent_id` field.
    pub async fn schedule_agent(&self, agent_id: AgentId) -> anyhow::Result<u64> {
        self.schedule_agent_with_overrides(agent_id, None, None)
            .await
            .map(|processed| processed.processed)
    }

    /// Schedule processing and return the last loop result.
    ///
    /// Subagent dispatch uses this to run a child with narrowed policy and a
    /// per-kind loop configuration while preserving the single-writer claim.
    ///
    /// Span name `agent{id=…}` is the canonical root of the runtime →
    /// agent-loop span chain; downstream spans (`worker`, `task{id}`,
    /// `turn`, `sampling`, `complete{model}`) all inherit from it so
    /// the visible prefix in the structured console transcript reads
    /// `agent{id}:worker:task{id}:turn{T}:sampling{I}:complete{model}`
    /// — every level adds new information.
    #[instrument(name = "agent", skip(self, agent_loop_config, policy), fields(id = %agent_id))]
    pub async fn schedule_agent_with_overrides(
        &self,
        agent_id: AgentId,
        agent_loop_config: Option<AgentLoopConfig>,
        policy: Option<PolicyConfig>,
    ) -> anyhow::Result<ProcessedAgent> {
        let status = self.store.get_agent_status(agent_id)?;
        if status != AgentStatus::Active {
            debug!(?status, "Agent not active, skipping");
            return Ok(ProcessedAgent::default());
        }

        if !self.store.has_pending_tx(agent_id)? {
            debug!("No pending transactions");
            return Ok(ProcessedAgent::default());
        }

        let Some(mut claim) = self.try_processing_claim(agent_id)? else {
            debug!("Agent already processing, skipping");
            return Ok(ProcessedAgent::default());
        };

        debug!("Processing claim acquired, constructing kernel for agent");

        let router = self.build_executor_router();
        let mut kernel_config = self.kernel_config.clone();
        if let Some(policy) = policy {
            kernel_config.policy = policy;
        }
        // The store-backed claim is the scheduler-side mitigation for
        // `Kernel::next_seq`'s read-before-append window: only one scheduler
        // kernel is constructed for an agent while a claim is held.
        // TODO(phase1-seq-reservation): replace this mitigation with a
        // store-backed sequence reservation if non-claim writers grow.
        let kernel = Arc::new(
            Kernel::new(
                self.store.clone(),
                self.provider.clone(),
                router,
                kernel_config,
                agent_id,
            )
            .map_err(|e| anyhow::anyhow!("kernel construction failed: {e}"))?,
        );

        let mut config = agent_loop_config.unwrap_or_else(|| self.agent_loop_config.clone());
        if let Some(ref mm) = self.memory_manager {
            config.observers.push(mm.turn_observer(agent_id, None));
        }
        let agent_loop = AgentLoop::new(config);

        let result = process_agent_detailed(agent_id, kernel, &agent_loop, &self.tools).await;
        let release_result = claim.release();

        match (result, release_result) {
            (Ok(processed), Ok(())) => {
                info!(processed = processed.processed, "Agent processing complete");
                Ok(processed)
            }
            (Ok(_), Err(e)) => {
                error!(error = %e, "Agent processing claim release failed");
                Err(e)
            }
            (Err(e), release) => {
                if let Err(release_error) = release {
                    error!(error = %release_error, "Agent processing claim release failed after error");
                }
                error!(error = %e, "Agent processing failed");
                Err(e)
            }
        }
    }

    /// Check if an agent is currently being processed.
    ///
    /// Returns `true` if the agent's store-backed processing claim is present.
    /// Retained for use by future status/health endpoints.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_agent_busy(&self, agent_id: AgentId) -> bool {
        self.store
            .is_agent_processing(agent_id)
            .unwrap_or_else(|e| {
                error!(error = %e, "Failed to read agent processing claim");
                false
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::{Transaction, TransactionType};
    use aura_reasoner::MockProvider;
    use aura_store::RocksStore;
    use bytes::Bytes;
    use std::time::Duration;

    fn create_test_scheduler() -> (Scheduler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> =
            Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("test"));
        let ws_dir = dir.path().join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let scheduler = Scheduler::new(store, provider, vec![], vec![], ws_dir, None);
        (scheduler, dir)
    }

    fn create_test_scheduler_with_provider(
        provider: Arc<dyn ModelProvider + Send + Sync>,
    ) -> (Scheduler, Arc<dyn Store>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> =
            Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let ws_dir = dir.path().join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let scheduler = Scheduler::new(store.clone(), provider, vec![], vec![], ws_dir, None);
        (scheduler, store, dir)
    }

    fn enqueue_prompt(store: &Arc<dyn Store>, agent_id: AgentId, prompt: &str) {
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from(prompt.to_owned()),
            None,
        );
        store.enqueue_tx(&tx).unwrap();
    }

    #[test]
    fn test_scheduler_creation() {
        let (_scheduler, _dir) = create_test_scheduler();
    }

    #[tokio::test]
    async fn test_schedule_agent_no_pending() {
        let (scheduler, _dir) = create_test_scheduler();
        let agent_id = AgentId::generate();
        let result = scheduler.schedule_agent(agent_id).await.unwrap();
        assert_eq!(result, 0, "No pending txs should process 0");
    }

    #[tokio::test]
    async fn test_schedule_paused_agent_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> =
            Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("test"));
        let ws_dir = dir.path().join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();

        let agent_id = AgentId::generate();
        store
            .set_agent_status(agent_id, AgentStatus::Paused)
            .unwrap();

        let scheduler = Scheduler::new(store, provider, vec![], vec![], ws_dir, None);
        let result = scheduler.schedule_agent(agent_id).await.unwrap();
        assert_eq!(result, 0, "Paused agents should be skipped");
    }

    #[tokio::test]
    async fn test_schedule_dead_agent_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Store> =
            Arc::new(RocksStore::open(dir.path().join("db"), false).unwrap());
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("test"));
        let ws_dir = dir.path().join("workspaces");
        std::fs::create_dir_all(&ws_dir).unwrap();

        let agent_id = AgentId::generate();
        store.set_agent_status(agent_id, AgentStatus::Dead).unwrap();

        let scheduler = Scheduler::new(store, provider, vec![], vec![], ws_dir, None);
        let result = scheduler.schedule_agent(agent_id).await.unwrap();
        assert_eq!(result, 0, "Dead agents should be skipped");
    }

    #[test]
    fn test_is_agent_busy_false_by_default() {
        let (scheduler, _dir) = create_test_scheduler();
        let agent_id = AgentId::generate();
        assert!(!scheduler.is_agent_busy(agent_id));
    }

    #[test]
    fn test_build_executor_router() {
        let (scheduler, _dir) = create_test_scheduler();
        let _router = scheduler.build_executor_router();
    }

    #[tokio::test]
    async fn test_concurrent_scheduler_calls_only_one_processes_pending_agent() {
        let provider = Arc::new(MockProvider::simple_response("processed").with_latency(150));
        let provider_for_assert = provider.clone();
        let (scheduler, store, _dir) = create_test_scheduler_with_provider(provider);
        let scheduler = Arc::new(scheduler);
        let agent_id = AgentId::generate();
        enqueue_prompt(&store, agent_id, "hello");

        let first = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.schedule_agent(agent_id).await })
        };

        tokio::time::timeout(Duration::from_secs(1), async {
            while !scheduler.is_agent_busy(agent_id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first scheduler call should acquire claim");

        let second = scheduler.schedule_agent(agent_id).await.unwrap();
        let first = first.await.unwrap().unwrap();

        assert_eq!(first + second, 1);
        assert!(
            [0, 1].contains(&first) && [0, 1].contains(&second),
            "one caller should process and one should skip"
        );
        assert_eq!(provider_for_assert.call_count(), 1);
        assert!(!scheduler.is_agent_busy(agent_id));
    }

    #[tokio::test]
    async fn test_busy_and_status_checks_do_not_block_while_processing() {
        let provider = Arc::new(MockProvider::simple_response("processed").with_latency(200));
        let (scheduler, store, _dir) = create_test_scheduler_with_provider(provider);
        let scheduler = Arc::new(scheduler);
        let agent_id = AgentId::generate();
        enqueue_prompt(&store, agent_id, "hello");

        let handle = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.schedule_agent(agent_id).await })
        };

        tokio::time::timeout(Duration::from_secs(1), async {
            while !scheduler.is_agent_busy(agent_id) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("scheduler should expose busy claim");

        let busy = tokio::time::timeout(Duration::from_millis(50), async {
            scheduler.is_agent_busy(agent_id)
        })
        .await
        .expect("busy check should not wait for processing");
        let status = tokio::time::timeout(Duration::from_millis(50), async {
            store.get_agent_status(agent_id)
        })
        .await
        .expect("status check should not wait for processing")
        .unwrap();

        assert!(busy);
        assert_eq!(status, AgentStatus::Active);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_processing_claim_released_after_error() {
        let provider = Arc::new(MockProvider::simple_response("unused"));
        let (scheduler, store, _dir) = create_test_scheduler_with_provider(provider);
        let agent_id = AgentId::generate();
        let tx = Transaction::new_chained(
            agent_id,
            TransactionType::UserPrompt,
            Bytes::from_static(&[0xff]),
            None,
        );
        store.enqueue_tx(&tx).unwrap();

        let result = scheduler.schedule_agent(agent_id).await;

        assert!(result.is_err());
        assert!(!scheduler.is_agent_busy(agent_id));
        assert!(
            scheduler.try_processing_claim(agent_id).unwrap().is_some(),
            "claim should be reusable after an error"
        );
    }

    #[test]
    fn test_scheduler_kernel_sequence_claim_invariant_documented() {
        let (scheduler, _dir) = create_test_scheduler();
        let agent_id = AgentId::generate();
        let mut claim = scheduler
            .try_processing_claim(agent_id)
            .unwrap()
            .expect("first claim should succeed");

        assert!(
            scheduler.try_processing_claim(agent_id).unwrap().is_none(),
            "scheduler must construct at most one Kernel per agent claim"
        );
        claim.release().unwrap();
    }
}
