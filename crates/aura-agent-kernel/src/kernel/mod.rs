//! Kernel implementation.
//!
//! ## Processing Invariants
//!
//! Every call to [`Kernel::process_direct`] or [`Kernel::process_dequeued`]
//! upholds the following guarantees:
//!
//! 1. **Deterministic context** — the context hash is derived solely from the
//!    incoming transaction and the record window loaded from the store.
//!    Re-processing the same inputs always yields the same context hash.
//!
//! 2. **Complete recording** — every intermediate artifact (proposals, policy
//!    decisions, actions, effects) is captured in the returned [`RecordEntry`]
//!    so the step can be replayed without a live reasoner or executor.
//!
//! 3. **Monotonic sequencing** — the internal counter guarantees strictly
//!    increasing sequence numbers without requiring the caller to supply them.
//!
//! The implementation is split across sibling files for readability:
//! - [`process`] — top-level `process_direct` / `process_dequeued` and the
//!   shared `process_tx` dispatcher plus the `System` capability-install
//!   decoder.
//! - [`tools`]   — `process_tool_proposal` and the batch `process_tools`.
//! - [`reason`]  — `reason` / `reason_streaming` and their timeout wrappers.
//! - [`stream`]  — the `ReasonStreamHandle` finalization handle.
//! - [`tests`]   — integration tests that exercise the full `Kernel` surface.

use crate::policy::{Policy, PolicyConfig};
use crate::replay::{ReplayConsumer, ReplayReport};
use crate::ExecutorRouter;
use async_trait::async_trait;
use aura_core_types::{AgentId, RecordEntry, RuntimeCapabilityInstall, ToolState};
use aura_core_modes::KernelMode;
use aura_plugin_hooks::PluginHookHost;
use aura_model_reasoner::ModelProvider;
use aura_store_db::Store;
use aura_store_record::DEFAULT_SUMMARY_CHUNK_BYTES;
use aura_store_snapshot::{NoopSnapshotStore, SnapshotStore};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

mod process;
mod reason;
mod stream;
#[cfg(test)]
mod tests;
mod tools;

pub use stream::ReasonStreamHandle;

// ============================================================================
// Configuration
// ============================================================================

/// Kernel configuration.
#[derive(Debug, Clone)]
pub struct KernelConfig {
    /// Size of record window for context
    pub record_window_size: usize,
    /// Policy configuration
    pub policy: PolicyConfig,
    /// Base workspace directory
    pub workspace_base: PathBuf,
    /// When true, use `workspace_base` directly instead of appending `agent_id`.
    pub use_workspace_base_as_root: bool,
    /// Phase 6b replay anchor.
    ///
    /// - `None` (default) — live mode: the kernel calls the live
    ///   reasoner and executor as usual.
    /// - `Some(from_seq)` — replay mode: at construction the kernel
    ///   runs a [`crate::ReplayConsumer`] over its store's record log
    ///   for `agent_id`, starting at `from_seq` (inclusive), to
    ///   replay every historical [`aura_core_types::RecordEntry`] in
    ///   forward `seq` order. The replay validates each entry's
    ///   `context_hash` against a fresh recomputation through
    ///   [`crate::hash_tx_with_window`] and, for AuditedLite
    ///   entries whose effect payloads were summarised, validates
    ///   the `full_hash` against [`KernelConfig::snapshot_store`].
    ///   On success the [`crate::ReplayReport`] is stashed on the
    ///   kernel and exposed via [`Kernel::replay_report`]. On
    ///   failure the constructor returns
    ///   [`crate::KernelError::Replay`] and no further processing
    ///   is permitted from that instance.
    ///
    /// Wiring this field replaces the Phase 6a `replay_mode: bool`
    /// placeholder that had no consumer.
    pub replay_from: Option<u64>,
    /// Snapshot-store backend consulted during replay for
    /// AuditedLite payloads. Defaults to
    /// [`aura_store_snapshot::NoopSnapshotStore`] (always returns
    /// `None`), so replay of an AuditedLite turn surfaces
    /// [`crate::ReplayError::SnapshotMissing`] unless a real
    /// content-addressed backend is wired here. The stub is the
    /// V1 default per the architecture plan.
    pub snapshot_store: Arc<dyn SnapshotStore>,
    /// Timeout for reasoner proposals in milliseconds.
    pub proposal_timeout_ms: u64,
    /// Per-tool execution timeout in milliseconds. Each individual tool in
    /// a batch is wrapped in a `tokio::time::timeout` with this budget; on
    /// expiration a failed `Effect` is emitted and the batch continues.
    pub tool_timeout_ms: u64,
    /// Live approval bridge for tri-state `ask` tool calls. When absent,
    /// `ask` resolves to a headless deny.
    pub tool_approval_prompter: Option<Arc<dyn ToolApprovalPrompter>>,
    /// Originating user id used when a live approval response is remembered
    /// forever into persisted user tool defaults.
    pub originating_user_id: Option<String>,
    /// Phase 6a kernel audit tier (`Audited` vs `AuditedLite`).
    ///
    /// Imported from [`aura_core_modes::KernelMode`]; the kernel does
    /// not own this enum. `Audited` (default) records full inline
    /// payloads; `AuditedLite` summarises payloads above
    /// [`KernelConfig::audited_lite_threshold_bytes`] into a
    /// `RecordPayload::Summary { head, tail, full_hash, full_len }`.
    /// Sequence numbers and `RecordKind` values are identical across
    /// the two modes — only the payload representation differs.
    pub kernel_mode: KernelMode,
    /// Payload-size threshold (bytes) above which `AuditedLite`
    /// switches from full inline to head/tail summary. Defaults to
    /// [`aura_store_record::DEFAULT_SUMMARY_CHUNK_BYTES`] (1 KiB).
    /// Ignored when `kernel_mode == KernelMode::Audited`.
    pub audited_lite_threshold_bytes: usize,
    /// Phase 10 carve-out 5b: optional [`PluginHookHost`] consulted
    /// by [`Kernel::resolve_prompt_verdict`] before the interactive
    /// `ToolApprovalPrompter` is invoked. A registered
    /// `PermissionRequest` handler that returns
    /// [`aura_plugin_hooks::HookOutcome::Approve`] short-circuits
    /// the prompt with `PolicyVerdict::Allow`, and `Deny` with
    /// `PolicyVerdict::Deny { reason }`. Any other outcome
    /// (`Continue` / `TimedOut`) falls through to the interactive
    /// prompt.
    ///
    /// `None` (default) preserves Phase 8 behaviour exactly — the
    /// kernel never fires `PermissionRequest` hooks and the
    /// interactive prompt is always reached when configured.
    pub plugin_hooks: Option<Arc<PluginHookHost>>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            record_window_size: 50,
            policy: PolicyConfig::default(),
            workspace_base: PathBuf::from("./workspaces"),
            use_workspace_base_as_root: false,
            replay_from: None,
            snapshot_store: Arc::new(NoopSnapshotStore),
            proposal_timeout_ms: 120_000,
            tool_timeout_ms: 120_000,
            tool_approval_prompter: None,
            originating_user_id: None,
            kernel_mode: KernelMode::Audited,
            audited_lite_threshold_bytes: DEFAULT_SUMMARY_CHUNK_BYTES,
            plugin_hooks: None,
        }
    }
}

// ============================================================================
// Result types
// ============================================================================

/// Output from a single tool execution within the kernel.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Tool use ID (from the model's `tool_use` block).
    pub tool_use_id: String,
    /// Result content (text or error message).
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
    /// Machine-readable result classification.
    pub kind: aura_core_types::ToolResultKind,
    /// Set when the kernel produced this output because the policy
    /// raised [`crate::PolicyVerdict::RequireApproval`].
    pub approval_required: Option<ApprovalRequiredInfo>,
    /// Optional per-file line diff for file-mutating tools (`fs_write`,
    /// `fs_edit`, `fs_delete`). Populated by the kernel boundary from
    /// the underlying [`aura_core_types::ToolResult::line_diff`] so the agent
    /// loop can attach accurate `lines_added` / `lines_removed` counts
    /// to its `FileChange` records without re-reading the filesystem.
    /// `None` for every other tool and for tool failures.
    pub line_diff: Option<aura_core_types::LineDiff>,
}

/// Details about a tool invocation that was denied because it needs an
/// out-of-band operator approval. Set on [`ToolOutput::approval_required`]
/// when the policy returns [`crate::PolicyVerdict::RequireApproval`].
#[derive(Debug, Clone)]
pub struct ApprovalRequiredInfo {
    /// Tool name, e.g. `"run_command"`.
    pub tool: String,
    /// Structured live prompt metadata for tri-state `ask` prompts.
    pub prompt: Option<PendingToolPrompt>,
}

/// Scope for remembering a live approval response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalRemember {
    /// Do not cache; the next call prompts again.
    Once,
    /// Cache for the current session.
    Session,
    /// Persist to the originating user's defaults.
    Forever,
}

/// Live approval response returned by a [`ToolApprovalPrompter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolApprovalResponse {
    pub decision: ToolState,
    pub remember: ToolApprovalRemember,
}

/// Structured prompt metadata emitted when a tool resolves to `ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToolPrompt {
    pub request_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub agent_id: AgentId,
    pub remember_options: Vec<ToolApprovalRemember>,
}

/// Error returned by a live approval bridge.
#[derive(Debug, thiserror::Error)]
pub enum ToolApprovalError {
    #[error("approval prompt could not be delivered")]
    DeliveryFailed,
    #[error("approval prompt was cancelled")]
    Cancelled,
    #[error("{0}")]
    Internal(String),
}

/// Bridge from the deterministic kernel to an attached interactive client.
#[async_trait]
pub trait ToolApprovalPrompter: Send + Sync + std::fmt::Debug {
    async fn prompt(
        &self,
        prompt: PendingToolPrompt,
    ) -> Result<ToolApprovalResponse, ToolApprovalError>;
}

/// Decision produced by [`Kernel::process_tool_proposal`] for a single
/// tool call. Surfaced on [`ProcessResult`] so HTTP routers (and any
/// other caller) can distinguish "needs operator sign-off" from
/// "permanently denied" without pattern-matching on the error string.
#[derive(Debug, Clone)]
pub enum ToolDecision {
    /// Tool call was authorized and executed.
    Allowed,
    /// Tool call was permanently denied by policy. No approval will
    /// unlock it.
    Denied {
        /// Human-readable reason pulled from the policy engine.
        reason: String,
    },
    /// Tool call is awaiting an out-of-band operator approval.
    NeedsApproval {
        /// Human-readable reason, e.g.
        /// `"Tool 'run_command' is set to ask"`.
        reason: String,
        /// Structured live prompt metadata for tri-state `ask` prompts.
        prompt: Option<PendingToolPrompt>,
    },
}

/// Result of processing a transaction.
#[derive(Debug)]
pub struct ProcessResult {
    /// The record entry created
    pub entry: RecordEntry,
    /// Tool output, if a tool was executed or denied
    pub tool_output: Option<ToolOutput>,
    /// Whether any actions failed
    pub had_failures: bool,
    /// Persisted runtime capability snapshot written by this transaction.
    pub runtime_capability_update: Option<RuntimeCapabilityInstall>,
    /// Whether the persisted runtime capability ledger should be cleared.
    pub clear_runtime_capabilities: bool,
    /// Structured policy decision, set when this `ProcessResult` came
    /// from a tool-proposal path. `None` for non-tool transactions.
    pub tool_decision: Option<ToolDecision>,
}

/// Result of a reasoning call.
#[derive(Debug)]
pub struct ReasonResult {
    /// The record entry created
    pub entry: RecordEntry,
    /// The model response
    pub response: aura_model_reasoner::ModelResponse,
}

// ============================================================================
// Kernel (concrete type with dynamic dispatch)
// ============================================================================

/// The deterministic kernel.
///
/// Uses `Arc<dyn Store>` and `Arc<dyn ModelProvider>` for dynamic dispatch,
/// removing the generic type parameters from the Phase-1 design.
pub struct Kernel {
    pub(super) store: Arc<dyn Store>,
    pub(super) provider: Arc<dyn ModelProvider + Send + Sync>,
    pub(super) executor: ExecutorRouter,
    pub(super) policy: Policy,
    pub(super) config: KernelConfig,
    /// Agent this kernel instance is bound to.
    pub agent_id: AgentId,
    pub(super) seq: Arc<Mutex<u64>>,
    /// Phase 6b replay outcome — `Some(report)` after a successful
    /// replay sweep at construction time when
    /// [`KernelConfig::replay_from`] is `Some`; `None` for live-mode
    /// kernels. Exposed read-only via [`Kernel::replay_report`].
    pub(super) replay_report: Option<ReplayReport>,
}

impl Kernel {
    /// Create a new kernel bound to a specific agent.
    ///
    /// Reads the current head sequence from the store so the internal counter
    /// starts at `head_seq + 1`. When [`KernelConfig::replay_from`] is
    /// `Some(from_seq)` the constructor additionally runs a
    /// [`ReplayConsumer`] over the existing record log for
    /// `agent_id`, starting at `from_seq` (inclusive), and stashes
    /// the resulting [`ReplayReport`]. The kernel will not begin
    /// accepting new transactions until that replay completes — see
    /// the [`KernelConfig::replay_from`] docs.
    ///
    /// # Errors
    ///
    /// - [`crate::KernelError::Store`] if the store cannot be read.
    /// - [`crate::KernelError::Replay`] if replay is configured and
    ///   the replay consumer surfaces a [`ReplayError`] (context
    ///   divergence, missing snapshot, store failure, etc.).
    pub fn new(
        store: Arc<dyn Store>,
        provider: Arc<dyn ModelProvider + Send + Sync>,
        executor: ExecutorRouter,
        config: KernelConfig,
        agent_id: AgentId,
    ) -> Result<Self, crate::KernelError> {
        let head_seq = store
            .get_head_seq(agent_id)
            .map_err(|e| crate::KernelError::Store(format!("get_head_seq: {e}")))?;
        let policy = Policy::new(config.policy.clone());

        // Phase 6b: run replay BEFORE the kernel becomes available for
        // any process_*() call. The constructor surfaces a typed
        // ReplayError so callers can distinguish replay failures from
        // generic kernel/store errors without string-matching.
        let replay_report = if let Some(from_seq) = config.replay_from {
            let consumer = ReplayConsumer::new(
                store.clone(),
                config.snapshot_store.clone(),
                agent_id,
                from_seq,
                config.record_window_size,
            );
            Some(consumer.run()?)
        } else {
            None
        };

        Ok(Self {
            store,
            provider,
            executor,
            policy,
            config,
            agent_id,
            seq: Arc::new(Mutex::new(head_seq + 1)),
            replay_report,
        })
    }

    /// Returns the replay report produced at construction time when
    /// [`KernelConfig::replay_from`] was set.
    ///
    /// `None` for live-mode kernels (no replay configured). The
    /// returned reference is read-only; the kernel does not re-run
    /// replay after the constructor.
    #[must_use]
    pub fn replay_report(&self) -> Option<&ReplayReport> {
        self.replay_report.as_ref()
    }

    /// Get a reference to the underlying store.
    pub fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// Read-only accessor for the kernel's `Policy`.
    ///
    /// Exposed for policy-focused tests and diagnostics. Pure observational
    /// surface — the policy's interior mutable state is still protected by
    /// its own synchronization.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    // -----------------------------------------------------------------------
    // Sequence helpers
    // -----------------------------------------------------------------------

    pub(super) fn next_seq(&self) -> Result<u64, crate::KernelError> {
        let head_seq = self
            .store
            .get_head_seq(self.agent_id)
            .map_err(|e| crate::KernelError::Store(format!("get_head_seq: {e}")))?;
        let next = head_seq + 1;
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq = next + 1;
        Ok(next)
    }

    pub(super) fn reserve_seq_range(&self, count: usize) -> Result<u64, crate::KernelError> {
        let head_seq = self
            .store
            .get_head_seq(self.agent_id)
            .map_err(|e| crate::KernelError::Store(format!("get_head_seq: {e}")))?;
        let base = head_seq + 1;
        let mut seq = self
            .seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq = base + count as u64;
        Ok(base)
    }

    pub(super) fn agent_workspace(&self) -> PathBuf {
        if self.config.use_workspace_base_as_root {
            self.config.workspace_base.clone()
        } else {
            self.config.workspace_base.join(self.agent_id.to_hex())
        }
    }

    // -----------------------------------------------------------------------
    // Context helpers
    // -----------------------------------------------------------------------

    pub(super) fn load_window(
        &self,
        next_seq: u64,
    ) -> Result<Vec<RecordEntry>, crate::KernelError> {
        let from_seq = next_seq.saturating_sub(self.config.record_window_size as u64);
        self.store
            .scan_record(self.agent_id, from_seq, self.config.record_window_size)
            .map_err(|e| crate::KernelError::Store(format!("scan_record: {e}")))
    }

    pub(super) fn load_runtime_capabilities(
        &self,
    ) -> Result<Option<RuntimeCapabilityInstall>, crate::KernelError> {
        self.store
            .get_runtime_capabilities(self.agent_id)
            .map_err(|e| crate::KernelError::Store(format!("get_runtime_capabilities: {e}")))
    }

    /// Returns the kernel's configured [`KernelMode`].
    #[must_use]
    pub fn kernel_mode(&self) -> KernelMode {
        self.config.kernel_mode
    }

    /// Returns `Some(threshold)` when [`KernelConfig::kernel_mode`] is
    /// [`KernelMode::AuditedLite`] (effect payloads above `threshold`
    /// bytes are summarised), or `None` for [`KernelMode::Audited`]
    /// (no summarisation; payloads stored verbatim).
    #[must_use]
    pub fn lite_payload_threshold(&self) -> Option<usize> {
        match self.config.kernel_mode {
            KernelMode::Audited => None,
            KernelMode::AuditedLite => Some(self.config.audited_lite_threshold_bytes),
        }
    }

    /// Encode `bytes` into a [`aura_store_record::RecordPayload`]
    /// honouring [`KernelConfig::kernel_mode`].
    ///
    /// - `KernelMode::Audited` always produces
    ///   [`aura_store_record::RecordPayload::Inline`] (full bytes).
    /// - `KernelMode::AuditedLite` produces
    ///   [`aura_store_record::RecordPayload::Summary`] when
    ///   `bytes.len() > audited_lite_threshold_bytes`, else
    ///   [`aura_store_record::RecordPayload::Inline`] (below
    ///   threshold). Sequence numbers + RecordKind are unaffected.
    #[must_use]
    pub fn encode_payload(&self, bytes: &[u8]) -> aura_store_record::RecordPayload {
        match self.config.kernel_mode {
            KernelMode::Audited => aura_store_record::RecordPayload::inline(bytes.to_vec()),
            KernelMode::AuditedLite => aura_store_record::summarize_payload(
                bytes,
                self.config.audited_lite_threshold_bytes,
            ),
        }
    }
}

/// Phase 6a consolidation: write a System-shaped `RecordEntry`
/// through the kernel crate.
///
/// Previously `aura-runtime::tool_permissions::append_agent_tool_permissions_entry`
/// constructed and appended this entry inline, bypassing the kernel
/// and violating the "kernel owns every `RecordEntry` write"
/// invariant. The body of the build-and-write now lives here; the
/// runtime retains responsibility for the scheduler's
/// `processing_claim` (a scheduler-side lock) which the kernel crate
/// would not be able to see without a layering violation.
///
/// On-disk shape: identical to the prior runtime path — same
/// `seq = head_seq + 1`, same window-hashed `context_hash`, same
/// `append_entry_direct` write path. Replay and audit consumers see
/// no diff.
///
/// # Errors
///
/// Returns [`KernelError::Store`] for any backend failure.
pub fn write_system_record(
    store: &Arc<dyn Store>,
    agent_id: AgentId,
    tx: aura_core_types::Transaction,
) -> Result<u64, crate::KernelError> {
    // The `seq` for a System record is derived from a `get_head_seq`
    // read that is NOT serialized with the actual `append_entry_direct`
    // write at the store level. When another writer for the same agent
    // commits between our read and our append (e.g. a SubagentSpawn
    // audit record racing the parent's own turn writes), the store's
    // pre-flight `assert_next_seq` rejects us with
    // [`StoreError::SequenceMismatch`]. That is a transient,
    // recompute-and-retry condition rather than a hard failure, so we
    // re-read the head and rebuild the entry on a bounded retry loop.
    const MAX_RETRIES: u32 = 16;
    for attempt in 0..=MAX_RETRIES {
        match append_system_record_once(store, agent_id, &tx) {
            Ok(seq) => return Ok(seq),
            Err(SystemRecordError::SequenceMismatch) if attempt < MAX_RETRIES => {
                // Brief backoff so the racing writer can land before we
                // recompute. Capped low because the audit append is on
                // the subagent spawn hot path.
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::from(attempt + 1).min(8),
                ));
            }
            Err(SystemRecordError::SequenceMismatch) => {
                return Err(crate::KernelError::Store(format!(
                    "append_entry_direct: sequence mismatch unresolved after {MAX_RETRIES} retries"
                )));
            }
            Err(SystemRecordError::Other(err)) => return Err(err),
        }
    }
    unreachable!("write_system_record retry loop always returns")
}

/// Internal error discriminator for [`write_system_record`]'s retry
/// loop: `SequenceMismatch` is the retryable case, everything else is
/// terminal.
enum SystemRecordError {
    SequenceMismatch,
    Other(crate::KernelError),
}

/// Single attempt to append a System record: read the head, hash the
/// transaction against the trailing window, and direct-append at
/// `head + 1`. Returns [`SystemRecordError::SequenceMismatch`] when the
/// store rejects the computed sequence so the caller can retry.
fn append_system_record_once(
    store: &Arc<dyn Store>,
    agent_id: AgentId,
    tx: &aura_core_types::Transaction,
) -> Result<u64, SystemRecordError> {
    let head = store.get_head_seq(agent_id).map_err(|e| {
        SystemRecordError::Other(crate::KernelError::Store(format!("get_head_seq: {e}")))
    })?;
    let from_seq = head.saturating_sub(49).max(1);
    let window = if head == 0 {
        Vec::new()
    } else {
        store.scan_record(agent_id, from_seq, 50).map_err(|e| {
            SystemRecordError::Other(crate::KernelError::Store(format!("scan_record: {e}")))
        })?
    };
    let context_hash = crate::context::hash_tx_with_window(tx, &window).map_err(|e| {
        SystemRecordError::Other(crate::KernelError::Internal(format!("context hash: {e}")))
    })?;
    let seq = head + 1;
    let entry = RecordEntry::builder(seq, tx.clone())
        .context_hash(context_hash)
        .build();
    match store.append_entry_direct(agent_id, seq, &entry) {
        Ok(()) => Ok(seq),
        Err(aura_store_db::StoreError::SequenceMismatch { .. }) => {
            Err(SystemRecordError::SequenceMismatch)
        }
        Err(e) => Err(SystemRecordError::Other(crate::KernelError::Store(format!(
            "append_entry_direct: {e}"
        )))),
    }
}
