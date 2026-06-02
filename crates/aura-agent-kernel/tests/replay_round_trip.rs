//! Phase 6b — `tests/replay_round_trip.rs`.
//!
//! Exit-criterion suite for Phase 6b replay wiring. Three scenarios
//! pin the contract documented in
//! `crates/aura-agent-kernel/src/replay.rs`:
//!
//! 1. **Happy path** — a real `Kernel` records a turn (a user-prompt
//!    plus a tool execution under a default-allow policy and a stub
//!    [`Executor`]). A fresh `Kernel` is then constructed against
//!    the same store with `replay_from = Some(0)`; replay runs at
//!    construction, the resulting [`ReplayReport`] matches the
//!    recorded decisions, and the final state hash equals the last
//!    entry's `context_hash`. Determinism is asserted by running
//!    replay twice and comparing the two reports byte-for-byte.
//!
//! 2. **`ReplayError::ContextDivergence`** — the test appends an
//!    extra `RecordEntry` whose `context_hash` deliberately diverges
//!    from `hash_tx_with_window`'s recomputation. Replay aborts with
//!    [`ReplayError::ContextDivergence`] carrying the diverging
//!    `seq`, expected (recorded) hash, and actual (recomputed) hash
//!    — surfaced via the typed [`crate::KernelError::Replay`] arm
//!    rather than a stringified diagnostic.
//!
//! 3. **`ReplayError::SnapshotMissing`** — a kernel in
//!    `KernelMode::AuditedLite` records a tool execution whose
//!    effect payload exceeds the configured size threshold and is
//!    therefore summarised into a
//!    [`aura_store_record::RecordPayload::Summary`] envelope.
//!    Replaying against the default [`NoopSnapshotStore`] (which
//!    never returns bytes) aborts with
//!    [`ReplayError::SnapshotMissing`] carrying the recorded
//!    `full_hash`.

use std::sync::Arc;

use async_trait::async_trait;
use aura_agent_kernel::{
    hash_tx_with_window, Executor, ExecutorError, ExecutorRouter, Kernel, KernelConfig,
    KernelError, ReplayError,
};
use aura_core_modes::KernelMode;
use aura_core_types::{
    Action, ActionKind, AgentId, ContextHash, Decision, Effect, EffectKind, EffectStatus,
    ProposalSet, RecordEntry, ToolProposal, ToolResult, Transaction, TransactionType,
};
use aura_model_reasoner::{MockProvider, ModelProvider};
use aura_store_db::{RocksStore, Store, WriteStore};
use aura_store_snapshot::{NoopSnapshotStore, SnapshotStore};
use tempfile::TempDir;

// =============================================================================
// Test-only `Executor` stubs
// =============================================================================

/// Stub executor that emits a committed `Agreement` effect whose
/// payload is a JSON-encoded [`ToolResult`] with `stdout = stdout`.
///
/// The effect's serialized payload must round-trip through
/// `decode_tool_effect` for the kernel's `had_failures` flag to be
/// `false`; emitting raw bytes here trips the AgentError fallback
/// inside the decoder and the replay tests below would never see
/// the "successful tool" surface.
///
/// `can_handle` matches every `ActionKind::Delegate` so the kernel's
/// policy gate (default `PolicyConfig` accepts `Delegate`) always
/// lands here.
#[derive(Debug)]
struct FixedPayloadExecutor {
    stdout: Vec<u8>,
}

impl FixedPayloadExecutor {
    fn new(stdout: Vec<u8>) -> Self {
        Self { stdout }
    }
}

#[async_trait]
impl Executor for FixedPayloadExecutor {
    async fn execute(
        &self,
        _ctx: &aura_agent_kernel::ExecuteContext,
        action: &Action,
    ) -> Result<Effect, ExecutorError> {
        let result = ToolResult::success("read_file", self.stdout.clone());
        let payload = serde_json::to_vec(&result)
            .map_err(|e| ExecutorError::ExecutionFailed(format!("serialise ToolResult: {e}")))?;
        Ok(Effect::new(
            action.action_id,
            EffectKind::Agreement,
            EffectStatus::Committed,
            payload,
        ))
    }

    fn can_handle(&self, action: &Action) -> bool {
        matches!(action.kind, ActionKind::Delegate)
    }

    fn name(&self) -> &'static str {
        "fixed-payload"
    }
}

// =============================================================================
// Test fixtures
// =============================================================================

struct LiveFixture {
    kernel: Kernel,
    store: Arc<dyn Store>,
    agent_id: AgentId,
    workspace: TempDir,
    _db: TempDir,
}

fn build_live_kernel(payload: Vec<u8>, kernel_mode: KernelMode) -> LiveFixture {
    let db_dir = TempDir::new().expect("temp dir");
    let ws_dir = TempDir::new().expect("temp dir");
    let agent_id = AgentId::generate();

    let store: Arc<dyn Store> =
        Arc::new(RocksStore::open(db_dir.path(), false).expect("open rocks store"));
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));

    let mut executor = ExecutorRouter::new();
    executor.add_executor(Arc::new(FixedPayloadExecutor::new(payload)));

    let config = KernelConfig {
        workspace_base: ws_dir.path().to_path_buf(),
        kernel_mode,
        // Force every AuditedLite effect path to summarise; the
        // default 1 KiB threshold is too coarse for fast tests.
        audited_lite_threshold_bytes: 16,
        ..KernelConfig::default()
    };

    let kernel = Kernel::new(store.clone(), provider, executor, config, agent_id)
        .expect("construct live kernel");

    LiveFixture {
        kernel,
        store,
        agent_id,
        workspace: ws_dir,
        _db: db_dir,
    }
}

/// Build a fresh kernel against the same store + agent + workspace,
/// with `replay_from = Some(from_seq)` and an explicit snapshot
/// store. The constructor runs the replay sweep synchronously and
/// returns the kernel (Ok) or a typed [`KernelError`] (Err).
fn build_replay_kernel(
    fixture: &LiveFixture,
    from_seq: u64,
    snapshot_store: Arc<dyn SnapshotStore>,
) -> Result<Kernel, KernelError> {
    let provider: Arc<dyn ModelProvider + Send + Sync> =
        Arc::new(MockProvider::simple_response("noop"));
    let config = KernelConfig {
        workspace_base: fixture.workspace.path().to_path_buf(),
        replay_from: Some(from_seq),
        snapshot_store,
        kernel_mode: fixture.kernel.kernel_mode(),
        audited_lite_threshold_bytes: 16,
        ..KernelConfig::default()
    };
    Kernel::new(
        fixture.store.clone(),
        provider,
        ExecutorRouter::new(),
        config,
        fixture.agent_id,
    )
}

// =============================================================================
// Scenario 1 — happy path
// =============================================================================

#[tokio::test]
async fn replay_reproduces_recorded_decisions_and_state_hash() {
    let fixture = build_live_kernel(b"small-result".to_vec(), KernelMode::Audited);

    // Record a user prompt (seq=1) and a tool execution (seq=2).
    fixture
        .kernel
        .process_direct(Transaction::user_prompt(
            fixture.agent_id,
            "hello world".as_bytes().to_vec(),
        ))
        .await
        .expect("process user prompt");

    let proposal = ToolProposal::new(
        "tool-use-replay",
        "read_file",
        serde_json::json!({ "path": "a.txt" }),
    );
    let tool_tx = Transaction::tool_proposal(fixture.agent_id, &proposal).expect("tool tx");
    let result = fixture
        .kernel
        .process_direct(tool_tx)
        .await
        .expect("process tool");
    assert!(
        !result.had_failures,
        "tool execution must succeed; tool_output={:?}",
        result.tool_output
    );
    assert_eq!(result.entry.seq, 2);

    // Snapshot the recorded entries so we can compare per-entry
    // decisions after replay.
    let recorded: Vec<RecordEntry> = fixture
        .store
        .scan_record(fixture.agent_id, 1, 10)
        .expect("scan recorded entries");
    assert_eq!(recorded.len(), 2, "fixture should have recorded 2 entries");

    // Replay 1.
    let replay_kernel_a = build_replay_kernel(&fixture, 0, Arc::new(NoopSnapshotStore))
        .expect("replay kernel A construction must succeed");
    let report_a = replay_kernel_a
        .replay_report()
        .expect("replay kernel must produce a report")
        .clone();

    assert_eq!(report_a.replayed_seqs, vec![1, 2]);
    assert_eq!(report_a.decisions.len(), 2);
    for (seq_idx, decision) in report_a.decisions.iter().enumerate() {
        assert_eq!(
            decision,
            &recorded[seq_idx].decision,
            "decision at seq {} must match record",
            seq_idx + 1
        );
    }
    assert_eq!(
        report_a.final_state_hash, recorded[1].context_hash,
        "final_state_hash must equal the last entry's context_hash"
    );

    // Determinism: replay 2 over the same store must produce a
    // byte-identical report.
    let replay_kernel_b = build_replay_kernel(&fixture, 0, Arc::new(NoopSnapshotStore))
        .expect("replay kernel B construction must succeed");
    let report_b = replay_kernel_b
        .replay_report()
        .expect("replay kernel must produce a report")
        .clone();

    assert_eq!(report_a.replayed_seqs, report_b.replayed_seqs);
    assert_eq!(report_a.decisions, report_b.decisions);
    assert_eq!(report_a.final_state_hash, report_b.final_state_hash);
}

// =============================================================================
// Scenario 2 — `ReplayError::ContextDivergence`
// =============================================================================

#[tokio::test]
async fn replay_aborts_on_context_divergence() {
    let fixture = build_live_kernel(b"small-result".to_vec(), KernelMode::Audited);

    // Genuine entry at seq=1 — context_hash is correct.
    fixture
        .kernel
        .process_direct(Transaction::user_prompt(
            fixture.agent_id,
            "first".as_bytes().to_vec(),
        ))
        .await
        .expect("process first user prompt");

    // Genuine entry at seq=2 — context_hash is correct.
    fixture
        .kernel
        .process_direct(Transaction::user_prompt(
            fixture.agent_id,
            "second".as_bytes().to_vec(),
        ))
        .await
        .expect("process second user prompt");

    // Forge a divergent entry at seq=3. We compute the prior window
    // through hash_tx_with_window for context, then write the entry
    // with an all-FF (`0xFF * 32`) context_hash that cannot possibly
    // match any recomputation.
    let prior_window: Vec<RecordEntry> = fixture
        .store
        .scan_record(fixture.agent_id, 1, 10)
        .expect("scan prior window");
    let bogus_tx = Transaction::user_prompt(fixture.agent_id, "diverging".as_bytes().to_vec());
    let recomputed =
        hash_tx_with_window(&bogus_tx, &prior_window).expect("recompute prior window hash");
    let bogus_hash = ContextHash::from([0xFFu8; 32]);
    assert_ne!(
        bogus_hash, recomputed,
        "the divergence test relies on the bogus hash differing from the recomputation"
    );
    let bogus_entry = RecordEntry::builder(3, bogus_tx.clone())
        .context_hash(bogus_hash)
        .proposals(ProposalSet::new())
        .decision(Decision::new())
        .build();

    let rocks_store = Arc::clone(&fixture.store);
    WriteStore::append_entry_direct(rocks_store.as_ref(), fixture.agent_id, 3, &bogus_entry)
        .expect("forge divergent entry");

    match build_replay_kernel(&fixture, 0, Arc::new(NoopSnapshotStore)) {
        Ok(_) => panic!("replay must abort on context divergence"),
        Err(KernelError::Replay(ReplayError::ContextDivergence {
            seq,
            expected,
            actual,
        })) => {
            assert_eq!(seq, 3, "diverging entry seq must be reported");
            assert_eq!(
                expected,
                hex::encode(bogus_hash.as_ref()),
                "expected hash must match the forged record's context_hash"
            );
            assert_eq!(
                actual,
                hex::encode(recomputed.as_ref()),
                "actual hash must match the canonical recomputation"
            );
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

// =============================================================================
// Scenario 3 — `ReplayError::SnapshotMissing`
// =============================================================================

#[tokio::test]
async fn replay_aborts_on_snapshot_missing_for_audited_lite_summary() {
    // A 64-byte payload comfortably exceeds the 16-byte threshold
    // configured in `build_live_kernel`, guaranteeing the kernel
    // summarises the effect payload into `RecordPayload::Summary`.
    let large_payload = vec![0x5Au8; 64];
    let fixture = build_live_kernel(large_payload, KernelMode::AuditedLite);

    let proposal = ToolProposal::new(
        "tool-use-lite",
        "read_file",
        serde_json::json!({ "path": "lite.txt" }),
    );
    let tool_tx =
        Transaction::tool_proposal(fixture.agent_id, &proposal).expect("tool proposal tx");
    // `had_failures` is intentionally NOT asserted: AuditedLite
    // summarisation rewrites the effect payload into a
    // `RecordPayload::Summary` JSON envelope BEFORE the kernel's
    // boundary `decode_tool_effect` call runs. The decode then
    // misclassifies the wrapper as an AgentError because it cannot
    // parse it as `ToolResult`. The replay test does not depend on
    // the `tool_output.is_error` flag — only on the persisted
    // effect carrying the `Summary` shape.
    let _ = fixture
        .kernel
        .process_direct(tool_tx)
        .await
        .expect("process tool");

    // Confirm the recorded effect was actually summarised — the
    // SnapshotMissing path only fires when the replay consumer
    // observes a `RecordPayload::Summary` envelope.
    let entries = fixture
        .store
        .scan_record(fixture.agent_id, 1, 10)
        .expect("scan entries");
    let summary_count = entries
        .iter()
        .flat_map(|e| &e.effects)
        .filter(|effect| {
            serde_json::from_slice::<aura_store_record::RecordPayload>(&effect.payload)
                .ok()
                .is_some_and(|payload| {
                    matches!(payload, aura_store_record::RecordPayload::Summary { .. })
                })
        })
        .count();
    assert_eq!(
        summary_count, 1,
        "AuditedLite + threshold=16 + 64-byte payload should summarise exactly one effect"
    );

    match build_replay_kernel(
        &fixture,
        0,
        Arc::new(NoopSnapshotStore), // no-op stub returns None for every fetch
    ) {
        Ok(_) => panic!("replay must abort when AuditedLite snapshot is missing"),
        Err(KernelError::Replay(ReplayError::SnapshotMissing { seq, full_hash })) => {
            assert_eq!(
                seq, 1,
                "the SnapshotMissing report must carry the seq of the summarised entry"
            );
            assert!(
                !full_hash.is_empty(),
                "the SnapshotMissing report must carry the recorded full_hash"
            );
        }
        Err(other) => panic!("unexpected error: {other:?}"),
    }
}

// =============================================================================
// Bonus — `replay_from` past head is a no-op (Ok with empty report)
// =============================================================================

#[tokio::test]
async fn replay_from_past_head_returns_empty_report() {
    let fixture = build_live_kernel(b"small-result".to_vec(), KernelMode::Audited);

    fixture
        .kernel
        .process_direct(Transaction::user_prompt(
            fixture.agent_id,
            "only entry".as_bytes().to_vec(),
        ))
        .await
        .expect("process user prompt");

    let replay_kernel = build_replay_kernel(&fixture, 100, Arc::new(NoopSnapshotStore))
        .expect("replay must succeed");
    let report = replay_kernel
        .replay_report()
        .expect("replay must produce a report");
    assert!(report.replayed_seqs.is_empty());
    assert!(report.decisions.is_empty());
    assert_eq!(report.final_state_hash, ContextHash::zero());
}

// =============================================================================
// Sanity — non-system Transaction types we touch above are still valid
// =============================================================================

#[test]
fn user_prompt_tx_type_is_user_prompt() {
    // Pin the invariant that `Transaction::user_prompt` produces
    // `TransactionType::UserPrompt`; the replay tests above rely on
    // this so the kernel's `process_tx` default-arm path records
    // them with the same shape every time.
    let agent = AgentId::generate();
    let tx = Transaction::user_prompt(agent, b"x".to_vec());
    assert_eq!(tx.tx_type, TransactionType::UserPrompt);
}
