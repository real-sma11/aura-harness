//! [`FleetSpawner`] — Phase 7b subagent spawn composition root.
//!
//! See the crate-level docs for the per-spawn ordering contract and
//! the SpawnMode taxonomy.

use std::sync::Arc;

use aura_agent_kernel::write_system_record;
use aura_agent_subagent::{
    DefaultDerivation, DerivationError, OverrideManifest, ParentContext, SubagentDerivation,
    SubagentOverrides,
};
use aura_core::{AgentId, SubagentExit, SubagentResult, Transaction, TransactionType};
use aura_core_modes::{ModeViolation, SpawnMode};
use aura_fleet_quota::{BudgetTicket, QuotaError, QuotaPool, QuotaRequest};
use aura_fleet_registry::{AgentSlot, FleetRegistry, RegistryError};
use aura_store::Store;
use bytes::Bytes;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};

use crate::handle::{BatchInner, BatchSpawn, DetachedSpawn, SpawnHandle};
use crate::lease::{DedupedSpawn, ParentLeaseRegistry};
use crate::orphan::{OrphanRecord, OrphanStore};
use crate::runner::{ChildRunContext, ChildRunError, ChildRunner};

/// Stable kind tag stamped on the JSON envelope written for every
/// successful spawn. Phase 7b adds the matching
/// [`aura_store_record::RecordKind::SubagentSpawn`] variant; the
/// audit-log record header now uses it directly while this constant
/// remains the in-payload discriminator for forward compatibility
/// with consumers that still inspect the envelope body.
pub const RECORD_KIND_SUBAGENT_SPAWN: &str = "subagent_spawn";

/// Wire shape of the `SubagentSpawn` audit record's payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubagentSpawnRecordPayload {
    /// Discriminator for the System-record envelope (`"subagent_spawn"`).
    pub kind: String,
    /// Parent agent that requested the spawn.
    pub parent_agent_id: AgentId,
    /// Freshly-allocated child agent id (pre-assigned by the spawner
    /// before the child runner is invoked).
    pub child_agent_id: AgentId,
    /// Manifest of explicit overrides the parent supplied. May be
    /// empty when the child inherits every field — see
    /// [`OverrideManifest::is_empty`].
    pub override_manifest: OverrideManifest,
    /// SpawnMode the parent's tool call resolved to.
    pub spawn_mode: SpawnMode,
}

/// Request handed to [`FleetSpawner::spawn`].
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Atomic snapshot of the parent's session state at spawn time.
    pub parent: ParentContext,
    /// Explicit overrides the parent's tool call supplied.
    pub overrides: SubagentOverrides,
    /// Initial prompt seeded into the child agent loop.
    pub prompt: String,
    /// Originating user id propagated through to the child's
    /// audit attribution + scheduler identity.
    pub originating_user_id: Option<String>,
    /// Caller-stamped tool-call id used to dedupe idempotent
    /// re-dispatches. `None` opts the spawn out of dedupe.
    pub tool_call_id: Option<String>,
    /// Optional caller-supplied cancellation token. The spawner
    /// forks a child token from this so cancelling the parent's
    /// token propagates into the child runner.
    pub cancellation: Option<CancellationToken>,
}

/// Errors returned by [`FleetSpawner::spawn`] / `spawn_batch`.
#[derive(Debug, Error)]
pub enum SpawnError {
    /// Parent mode does not permit spawning (Plan/Ask/Debug).
    /// Fast-failed before any resource acquisition.
    #[error("spawn rejected: parent mode disallows spawning ({0})")]
    ModeViolation(#[from] ModeViolation),

    /// `aura-agent-subagent::derive_subagent` rejected the request
    /// (depth exceeded, mode/permission widening, etc.).
    #[error("spawn rejected by derivation: {0}")]
    Derivation(#[from] DerivationError),

    /// Quota acquisition failed.
    #[error("spawn rejected by quota: {0}")]
    Quota(#[from] QuotaError),

    /// `aura-agent-kernel::write_system_record` failed.
    #[error("spawn rejected by audit kernel: {0}")]
    Audit(String),

    /// `FleetRegistry::register` failed (e.g. duplicate id).
    #[error("spawn rejected by registry: {0}")]
    Registry(#[from] RegistryError),

    /// The pluggable child runner errored.
    #[error("spawn child runner failed: {0}")]
    Child(#[from] ChildRunError),

    /// Orphan handoff failed to write the durable orphan record.
    #[error("spawn orphan handoff failed: {0}")]
    Orphan(String),

    /// Serde failure assembling the `SubagentSpawn` audit payload.
    #[error("spawn audit payload serialization failed: {0}")]
    Serialization(String),
}

/// Construction config for [`FleetSpawner`].
#[derive(Debug, Clone)]
pub struct FleetSpawnerConfig {
    /// Quota request shape applied to every spawn — concurrent-tool
    /// ceiling forwarded into [`QuotaRequest::max_concurrent_tools`].
    pub max_concurrent_tools: u32,
    /// Fleet-wide cancellation token. When this token fires the
    /// spawner forks a per-child cancel from it so a global shutdown
    /// gracefully propagates into every detached / batch child.
    /// `None` disables fleet-shutdown propagation (default).
    pub fleet_shutdown: Option<CancellationToken>,
}

impl Default for FleetSpawnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_tools: 4,
            fleet_shutdown: None,
        }
    }
}

/// Composition root for subagent spawn. See the crate-level docs
/// for the per-spawn ordering contract.
pub struct FleetSpawner {
    store: Arc<dyn Store>,
    registry: Arc<FleetRegistry>,
    quota: Arc<QuotaPool>,
    leases: Arc<ParentLeaseRegistry>,
    orphans: Arc<OrphanStore>,
    derivation: Arc<dyn SubagentDerivation>,
    child_runner: Arc<dyn ChildRunner>,
    config: FleetSpawnerConfig,
}

impl FleetSpawner {
    /// Construct a [`FleetSpawner`] with a custom derivation.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn Store>,
        registry: Arc<FleetRegistry>,
        quota: Arc<QuotaPool>,
        leases: Arc<ParentLeaseRegistry>,
        orphans: Arc<OrphanStore>,
        derivation: Arc<dyn SubagentDerivation>,
        child_runner: Arc<dyn ChildRunner>,
        config: FleetSpawnerConfig,
    ) -> Self {
        Self {
            store,
            registry,
            quota,
            leases,
            orphans,
            derivation,
            child_runner,
            config,
        }
    }

    /// Construct a [`FleetSpawner`] using the bundled
    /// [`DefaultDerivation`].
    #[must_use]
    pub fn with_default_derivation(
        store: Arc<dyn Store>,
        registry: Arc<FleetRegistry>,
        quota: Arc<QuotaPool>,
        leases: Arc<ParentLeaseRegistry>,
        orphans: Arc<OrphanStore>,
        child_runner: Arc<dyn ChildRunner>,
        config: FleetSpawnerConfig,
    ) -> Self {
        Self::new(
            store,
            registry,
            quota,
            leases,
            orphans,
            Arc::new(DefaultDerivation::default()),
            child_runner,
            config,
        )
    }

    /// Read-only access to the orphan store.
    #[must_use]
    pub fn orphan_store(&self) -> Arc<OrphanStore> {
        self.orphans.clone()
    }

    /// Spawn a single subagent. Dispatches into [`SpawnMode::Wait`]
    /// or [`SpawnMode::Detached`]; batch dispatch goes through
    /// [`Self::spawn_batch`].
    ///
    /// # Errors
    ///
    /// See [`SpawnError`].
    #[instrument(
        skip(self, request),
        fields(parent_agent_id = %request.parent.agent_id, mode = ?mode)
    )]
    pub async fn spawn(
        &self,
        request: SpawnRequest,
        mode: SpawnMode,
    ) -> Result<SpawnHandle, SpawnError> {
        // (1) Parent-mode gate. Plan/Ask/Debug short-circuit before
        //     any lease / quota / audit work.
        if !request.parent.mode.allows_spawn() {
            debug!(
                parent_mode = ?request.parent.mode,
                "fleet spawner: parent mode forbids spawning"
            );
            return Err(SpawnError::ModeViolation(ModeViolation::SpawnNotAllowed));
        }

        // (2) Idempotent dedupe — same (parent, tool_call_id) within
        //     the dedupe window short-circuits to the cached
        //     outcome without producing a duplicate child.
        if let Some(tool_call_id) = request.tool_call_id.clone() {
            if let Some(cached) = self
                .leases
                .lookup_dedupe(request.parent.agent_id, &tool_call_id)
            {
                debug!(
                    parent_agent_id = %request.parent.agent_id,
                    tool_call_id,
                    "fleet spawner: dedupe hit — returning cached outcome"
                );
                return Ok(dedupe_to_handle(cached, mode));
            }
        }

        let parent_agent_id = request.parent.agent_id;
        let parent_chain = request.parent.lineage.chain.clone();
        let lease = self.leases.acquire(parent_agent_id).await;
        debug!("fleet spawner: lease acquired");

        // (3) Derivation — runs depth + mode + permission validation.
        let (spec, manifest) = self
            .derivation
            .derive(&request.parent, request.overrides.clone())?;

        debug!(
            depth = spec.depth,
            kernel_mode = ?spec.kernel_mode,
            override_count = manifest.applied.len(),
            "fleet spawner: spec derived"
        );

        // (4) Pre-allocate the child agent id BEFORE quota / audit /
        //     dispatch so every downstream consumer sees the same id.
        let child_agent_id = AgentId::generate();
        let child_depth = u8::try_from(spec.depth).unwrap_or(u8::MAX);

        // (5) Quota acquire. RAII ticket — released on drop when the
        //     child loop completes.
        let ticket = self.quota.try_acquire(QuotaRequest {
            agent_id: parent_agent_id,
            child_depth,
            max_iterations: spec.budget.max_iterations,
            max_concurrent_tools: self.config.max_concurrent_tools,
            token_budget: Some(u64::from(spec.budget.max_tokens)),
        })?;

        // (6) SubagentSpawn audit record write under the parent's
        //     lease (linearised seq numbers).
        let manifest_payload = SubagentSpawnRecordPayload {
            kind: RECORD_KIND_SUBAGENT_SPAWN.to_string(),
            parent_agent_id,
            child_agent_id,
            override_manifest: manifest.clone(),
            spawn_mode: mode,
        };
        let manifest_bytes = serde_json::to_vec(&manifest_payload)
            .map_err(|e| SpawnError::Serialization(format!("manifest: {e}")))?;
        let audit_tx = Transaction::new_chained(
            parent_agent_id,
            TransactionType::System,
            Bytes::from(manifest_bytes),
            None,
        );
        let seq = write_system_record(&self.store, parent_agent_id, audit_tx)
            .map_err(|e| SpawnError::Audit(e.to_string()))?;
        info!(
            seq,
            child_agent_id = %child_agent_id,
            override_count = manifest.applied.len(),
            mode = ?mode,
            "fleet spawner: SubagentSpawn audit record appended"
        );

        // (7) Registry slot.
        let slot = AgentSlot::new(
            child_agent_id,
            Some(parent_agent_id),
            spec.mode,
            spec.kernel_mode,
            spec.permissions.clone(),
        );
        self.registry.register(slot)?;

        // (8) Drop the lease — the audit record is appended and the
        //     registry slot is in place. The actual child execution
        //     does NOT need to hold the parent's audit-append lease;
        //     subsequent spawns from the same parent are free to
        //     proceed.
        drop(lease);

        // (9) Build the per-child cancellation token. The parent's
        //     token cancels Wait/Batch children but NOT Detached;
        //     the fleet shutdown token always propagates.
        let cancellation = build_cancellation(
            mode,
            request.cancellation.as_ref(),
            self.config.fleet_shutdown.as_ref(),
        );

        // (10) Dispatch per spawn mode.
        let runner = self.child_runner.clone();
        let registry = self.registry.clone();
        let orphans = self.orphans.clone();
        let dedupe_key = request.tool_call_id.clone();
        let leases_for_dedupe = self.leases.clone();
        let prompt = request.prompt;
        let originating_user_id = request.originating_user_id.clone();
        let spec_for_runner = spec.clone();

        match mode {
            SpawnMode::Wait => {
                let result = runner
                    .run(ChildRunContext {
                        spec: spec_for_runner,
                        prompt,
                        originating_user_id,
                        parent_agent_id,
                        parent_chain,
                        cancellation,
                        preassigned_agent_id: child_agent_id,
                    })
                    .await?;
                drop(ticket);
                let _ = registry.set_state(child_agent_id, state_for_exit(&result.exit));
                if let Some(key) = dedupe_key {
                    leases_for_dedupe.record_dedupe(
                        parent_agent_id,
                        key,
                        DedupedSpawn::WaitResult(result.clone()),
                    );
                }
                Ok(SpawnHandle::Completed(result))
            }
            SpawnMode::Detached => {
                // Write the orphan record immediately — a detached
                // child is observable via `aura agents inspect` even
                // while its parent is still alive.
                let orphan_record = OrphanRecord {
                    agent_id: child_agent_id,
                    parent_lineage: parent_chain.clone(),
                    mode: spec.mode,
                    kernel_mode: spec.kernel_mode,
                    spawn_mode: SpawnMode::Detached,
                    spawned_at: Utc::now(),
                    kind: spec.subagent_type.clone(),
                    model_id: Some(spec.model_id.clone()),
                    originating_user_id: originating_user_id.clone(),
                };
                orphans
                    .write(&orphan_record)
                    .map_err(|e| SpawnError::Orphan(e.to_string()))?;

                let (result_tx, result_rx) = oneshot::channel::<SubagentResult>();
                let registry_clone = registry.clone();
                let orphans_for_task = orphans.clone();
                let runner_clone = runner.clone();
                let cancellation_clone = cancellation.clone();
                tokio::spawn(async move {
                    let outcome = runner_clone
                        .run(ChildRunContext {
                            spec: spec_for_runner,
                            prompt,
                            originating_user_id,
                            parent_agent_id,
                            parent_chain,
                            cancellation: cancellation_clone,
                            preassigned_agent_id: child_agent_id,
                        })
                        .await;
                    let result = match outcome {
                        Ok(r) => r,
                        Err(err) => SubagentResult {
                            child_agent_id: Some(child_agent_id),
                            final_message: String::new(),
                            total_input_tokens: 0,
                            total_output_tokens: 0,
                            files_changed: Vec::new(),
                            exit: SubagentExit::Failed {
                                reason: err.to_string(),
                            },
                        },
                    };
                    let _ = registry_clone.set_state(child_agent_id, state_for_exit(&result.exit));
                    let _ = orphans_for_task.write(&OrphanRecord {
                        agent_id: child_agent_id,
                        parent_lineage: orphan_record.parent_lineage.clone(),
                        mode: orphan_record.mode,
                        kernel_mode: orphan_record.kernel_mode,
                        spawn_mode: SpawnMode::Detached,
                        spawned_at: orphan_record.spawned_at,
                        kind: orphan_record.kind.clone(),
                        model_id: orphan_record.model_id.clone(),
                        originating_user_id: orphan_record.originating_user_id.clone(),
                    });
                    let _ = result_tx.send(result);
                    drop(ticket);
                });

                if let Some(key) = dedupe_key {
                    leases_for_dedupe.record_dedupe(
                        parent_agent_id,
                        key,
                        DedupedSpawn::AgentIds(vec![child_agent_id]),
                    );
                }

                Ok(SpawnHandle::Detached(DetachedSpawn {
                    agent_id: child_agent_id,
                    result_rx: Some(result_rx),
                }))
            }
            SpawnMode::Batch => {
                // `Batch` here is degenerate — a single-element
                // batch with the default `JoinPolicy`. The proper
                // multi-child entry point is `spawn_batch`.
                let policy = spec.join_policy;
                let handles = self.run_batch_single(
                    spec_for_runner,
                    prompt,
                    originating_user_id,
                    parent_agent_id,
                    parent_chain,
                    cancellation,
                    child_agent_id,
                    ticket,
                );
                let batch = BatchSpawn {
                    agent_ids: vec![child_agent_id],
                    policy,
                    inner: handles,
                };
                if let Some(key) = dedupe_key {
                    leases_for_dedupe.record_dedupe(
                        parent_agent_id,
                        key,
                        DedupedSpawn::AgentIds(vec![child_agent_id]),
                    );
                }
                Ok(SpawnHandle::Batch(batch))
            }
        }
    }

    /// Spawn a batch of subagents under the requested
    /// [`aura_core_modes::JoinPolicy`].
    ///
    /// Each [`SpawnRequest`] in `requests` is derived + admitted
    /// independently; failures during a single request are surfaced
    /// inside the [`BatchOutcome`] rather than aborting the whole
    /// call. The batch handle's join policy MUST be passed via the
    /// `policy` argument so the call site is explicit about its
    /// semantic choice (rather than reading it from each request's
    /// spec).
    ///
    /// # Errors
    ///
    /// Returns [`SpawnError`] only for failures that prevent the
    /// batch from being assembled at all (e.g. a derivation failure
    /// on the FIRST request). Per-child failures are aggregated
    /// inside the returned [`BatchSpawn`].
    pub async fn spawn_batch(
        &self,
        requests: Vec<SpawnRequest>,
        policy: aura_core_modes::JoinPolicy,
    ) -> Result<BatchSpawn, SpawnError> {
        let mut handles = Vec::with_capacity(requests.len());
        let mut cancellations = Vec::with_capacity(requests.len());
        let mut agent_ids = Vec::with_capacity(requests.len());
        for request in requests {
            let parent_agent_id = request.parent.agent_id;
            if !request.parent.mode.allows_spawn() {
                return Err(SpawnError::ModeViolation(ModeViolation::SpawnNotAllowed));
            }
            let lease = self.leases.acquire(parent_agent_id).await;
            let (spec, manifest) = self
                .derivation
                .derive(&request.parent, request.overrides.clone())?;
            let child_agent_id = AgentId::generate();
            let child_depth = u8::try_from(spec.depth).unwrap_or(u8::MAX);
            let ticket = self.quota.try_acquire(QuotaRequest {
                agent_id: parent_agent_id,
                child_depth,
                max_iterations: spec.budget.max_iterations,
                max_concurrent_tools: self.config.max_concurrent_tools,
                token_budget: Some(u64::from(spec.budget.max_tokens)),
            })?;
            let manifest_payload = SubagentSpawnRecordPayload {
                kind: RECORD_KIND_SUBAGENT_SPAWN.to_string(),
                parent_agent_id,
                child_agent_id,
                override_manifest: manifest.clone(),
                spawn_mode: SpawnMode::Batch,
            };
            let manifest_bytes = serde_json::to_vec(&manifest_payload)
                .map_err(|e| SpawnError::Serialization(format!("manifest: {e}")))?;
            let audit_tx = Transaction::new_chained(
                parent_agent_id,
                TransactionType::System,
                Bytes::from(manifest_bytes),
                None,
            );
            write_system_record(&self.store, parent_agent_id, audit_tx)
                .map_err(|e| SpawnError::Audit(e.to_string()))?;
            self.registry.register(AgentSlot::new(
                child_agent_id,
                Some(parent_agent_id),
                spec.mode,
                spec.kernel_mode,
                spec.permissions.clone(),
            ))?;
            drop(lease);

            let cancellation = build_cancellation(
                if policy == aura_core_modes::JoinPolicy::Abandon {
                    SpawnMode::Detached
                } else {
                    SpawnMode::Batch
                },
                request.cancellation.as_ref(),
                self.config.fleet_shutdown.as_ref(),
            );

            agent_ids.push(child_agent_id);

            match policy {
                aura_core_modes::JoinPolicy::Abandon => {
                    // Write orphan record + fire-and-forget.
                    let orphan_record = OrphanRecord {
                        agent_id: child_agent_id,
                        parent_lineage: request.parent.lineage.chain.clone(),
                        mode: spec.mode,
                        kernel_mode: spec.kernel_mode,
                        spawn_mode: SpawnMode::Batch,
                        spawned_at: Utc::now(),
                        kind: spec.subagent_type.clone(),
                        model_id: Some(spec.model_id.clone()),
                        originating_user_id: request.originating_user_id.clone(),
                    };
                    self.orphans
                        .write(&orphan_record)
                        .map_err(|e| SpawnError::Orphan(e.to_string()))?;
                    let runner = self.child_runner.clone();
                    let registry = self.registry.clone();
                    let parent_chain = request.parent.lineage.chain.clone();
                    let prompt = request.prompt;
                    let originating_user_id = request.originating_user_id.clone();
                    tokio::spawn(async move {
                        let outcome = runner
                            .run(ChildRunContext {
                                spec,
                                prompt,
                                originating_user_id,
                                parent_agent_id,
                                parent_chain,
                                cancellation,
                                preassigned_agent_id: child_agent_id,
                            })
                            .await;
                        let _ = match outcome {
                            Ok(r) => registry.set_state(child_agent_id, state_for_exit(&r.exit)),
                            Err(_) => registry
                                .set_state(child_agent_id, aura_fleet_registry::AgentState::Failed),
                        };
                        drop(ticket);
                    });
                }
                aura_core_modes::JoinPolicy::All | aura_core_modes::JoinPolicy::Any => {
                    let runner = self.child_runner.clone();
                    let registry = self.registry.clone();
                    let parent_chain = request.parent.lineage.chain.clone();
                    let prompt = request.prompt;
                    let originating_user_id = request.originating_user_id.clone();
                    let cancellation_for_task = cancellation.clone();
                    cancellations.push(cancellation);
                    let handle = tokio::spawn(async move {
                        let outcome = runner
                            .run(ChildRunContext {
                                spec,
                                prompt,
                                originating_user_id,
                                parent_agent_id,
                                parent_chain,
                                cancellation: cancellation_for_task,
                                preassigned_agent_id: child_agent_id,
                            })
                            .await;
                        let result = match outcome {
                            Ok(r) => r,
                            Err(err) => SubagentResult {
                                child_agent_id: Some(child_agent_id),
                                final_message: String::new(),
                                total_input_tokens: 0,
                                total_output_tokens: 0,
                                files_changed: Vec::new(),
                                exit: SubagentExit::Failed {
                                    reason: err.to_string(),
                                },
                            },
                        };
                        let _ = registry.set_state(child_agent_id, state_for_exit(&result.exit));
                        drop(ticket);
                        Ok::<_, SpawnError>(result)
                    });
                    handles.push(handle);
                }
            }
        }

        let inner = match policy {
            aura_core_modes::JoinPolicy::All => BatchInner::All(handles),
            aura_core_modes::JoinPolicy::Any => BatchInner::Any {
                children: handles,
                cancellations,
            },
            aura_core_modes::JoinPolicy::Abandon => BatchInner::Abandon,
        };
        Ok(BatchSpawn {
            agent_ids,
            policy,
            inner,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn run_batch_single(
        &self,
        spec: aura_agent_subagent::SubagentSpec,
        prompt: String,
        originating_user_id: Option<String>,
        parent_agent_id: AgentId,
        parent_chain: Vec<AgentId>,
        cancellation: CancellationToken,
        child_agent_id: AgentId,
        ticket: BudgetTicket,
    ) -> BatchInner {
        let runner = self.child_runner.clone();
        let registry = self.registry.clone();
        let handle = tokio::spawn(async move {
            let outcome = runner
                .run(ChildRunContext {
                    spec,
                    prompt,
                    originating_user_id,
                    parent_agent_id,
                    parent_chain,
                    cancellation,
                    preassigned_agent_id: child_agent_id,
                })
                .await;
            let result = match outcome {
                Ok(r) => r,
                Err(err) => SubagentResult {
                    child_agent_id: Some(child_agent_id),
                    final_message: String::new(),
                    total_input_tokens: 0,
                    total_output_tokens: 0,
                    files_changed: Vec::new(),
                    exit: SubagentExit::Failed {
                        reason: err.to_string(),
                    },
                },
            };
            let _ = registry.set_state(child_agent_id, state_for_exit(&result.exit));
            drop(ticket);
            Ok::<_, SpawnError>(result)
        });
        BatchInner::All(vec![handle])
    }
}

/// Promote a cached dedupe outcome back into a [`SpawnHandle`]. The
/// re-issued handle is intentionally weaker than the original: the
/// `Detached` variant carries `None` for `result_rx` because the
/// oneshot receiver was consumed by the first caller.
fn dedupe_to_handle(cached: DedupedSpawn, mode: SpawnMode) -> SpawnHandle {
    match (cached, mode) {
        (DedupedSpawn::WaitResult(result), _) => SpawnHandle::Completed(result),
        (DedupedSpawn::AgentIds(ids), SpawnMode::Detached) => {
            let agent_id = ids.first().copied().unwrap_or_else(AgentId::generate);
            SpawnHandle::Detached(DetachedSpawn {
                agent_id,
                result_rx: None,
            })
        }
        (DedupedSpawn::AgentIds(ids), _) => SpawnHandle::Batch(BatchSpawn {
            agent_ids: ids,
            policy: aura_core_modes::JoinPolicy::default(),
            inner: BatchInner::Abandon,
        }),
    }
}

/// Forks the per-child cancellation token from the parent's optional
/// token + the fleet's shutdown token, per the SpawnMode rules:
///
/// - `Wait` / `Batch`: parent's token + fleet shutdown both
///   propagate.
/// - `Detached`: ONLY the fleet shutdown propagates; the parent's
///   token is intentionally ignored so a parent that exits does not
///   cancel its detached children.
fn build_cancellation(
    mode: SpawnMode,
    parent_token: Option<&CancellationToken>,
    fleet_shutdown: Option<&CancellationToken>,
) -> CancellationToken {
    let child = CancellationToken::new();
    if let Some(shutdown) = fleet_shutdown {
        let child_clone = child.clone();
        let shutdown_clone = shutdown.clone();
        tokio::spawn(async move {
            shutdown_clone.cancelled().await;
            child_clone.cancel();
        });
    }
    if matches!(mode, SpawnMode::Wait | SpawnMode::Batch) {
        if let Some(parent) = parent_token {
            let child_clone = child.clone();
            let parent_clone = parent.clone();
            tokio::spawn(async move {
                parent_clone.cancelled().await;
                child_clone.cancel();
            });
        }
    }
    child
}

fn state_for_exit(exit: &SubagentExit) -> aura_fleet_registry::AgentState {
    match exit {
        SubagentExit::Completed => aura_fleet_registry::AgentState::Done,
        SubagentExit::Cancelled | SubagentExit::Rejected { .. } => {
            aura_fleet_registry::AgentState::Cancelled
        }
        SubagentExit::Failed { .. } | SubagentExit::Timeout => {
            aura_fleet_registry::AgentState::Failed
        }
    }
}

/// Promote a detached / batch-abandoned child to an orphan, writing
/// the durable orphan record + a `ChildOrphanedByParentDeath` audit
/// record under the parent's agent log.
///
/// This is invoked by callers that detect parent death (drop guard,
/// task-future cancellation). The orphan store write is idempotent;
/// the audit record write goes through
/// [`aura_agent_kernel::write_system_record`] so a single sequence
/// number is assigned per orphan promotion.
///
/// # Errors
///
/// Returns [`SpawnError::Orphan`] on orphan-store I/O failure and
/// [`SpawnError::Audit`] on kernel record write failure.
pub fn promote_to_orphan(
    store: &Arc<dyn Store>,
    orphans: &OrphanStore,
    record: &OrphanRecord,
) -> Result<(), SpawnError> {
    let parent = record
        .parent_lineage
        .last()
        .copied()
        .unwrap_or(record.agent_id);
    let agent_id = record.agent_id;
    orphans
        .write(record)
        .map_err(|e| SpawnError::Orphan(e.to_string()))?;
    let audit_payload = serde_json::json!({
        "kind": "child_orphaned_by_parent_death",
        "parent_agent_id": parent,
        "child_agent_id": agent_id,
    });
    let bytes = serde_json::to_vec(&audit_payload)
        .map_err(|e| SpawnError::Serialization(format!("orphan payload: {e}")))?;
    let tx = Transaction::new_chained(parent, TransactionType::System, Bytes::from(bytes), None);
    write_system_record(store, parent, tx).map_err(|e| SpawnError::Audit(e.to_string()))?;
    warn!(
        agent_id = %agent_id,
        parent_agent_id = %parent,
        "fleet spawner: child promoted to orphan"
    );
    Ok(())
}
