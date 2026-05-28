//! # aura-fleet-spawn
//!
//! Layer: fleet
//!
//! Spawn mechanics for subagents. Phase 7b extends the Phase 7a
//! [`FleetSpawner::spawn`] composition seam with the full
//! [`SpawnMode`] taxonomy (`Wait` / `Detached` / `Batch`), the
//! [`aura_core_modes::JoinPolicy`] state machine, idempotent dedupe
//! by `(parent_id, tool_call_id)`, orphan handoff under
//! [`OrphanStore`], and RAII [`aura_fleet_quota::BudgetTicket`]
//! enforcement.
//!
//! Phase 7a's single composition seam survives unchanged for `Wait`
//! callers (the task-tool insta snapshot is byte-identical); the new
//! modes add additional entry points without disturbing the
//! existing contract.
//!
//! ## Order of operations (per call)
//!
//! 1. Parent mode gate ([`aura_core_modes::AgentMode::allows_spawn`]).
//! 2. Dedupe lookup (`(parent_agent_id, tool_call_id)` LRU on
//!    [`ParentLeaseRegistry`]).
//! 3. Per-parent audit-append lease acquire.
//! 4. Derivation (`aura-agent-subagent::derive_subagent`).
//! 5. Child agent id pre-allocation.
//! 6. Quota acquire ([`aura_fleet_quota::QuotaPool::try_acquire`]).
//! 7. `SubagentSpawn` audit record write under the lease.
//! 8. `FleetRegistry` slot insert.
//! 9. Lease release (subsequent spawns from the same parent may
//!    proceed even while the child is still running).
//! 10. Per-mode dispatch:
//!    - `Wait`: run child runner to completion in-call, cache result
//!      for dedupe, return `SpawnHandle::Completed`.
//!    - `Detached`: write orphan record, spawn background tokio task,
//!      return `SpawnHandle::Detached` with `(agent_id, result_rx)`.
//!    - `Batch` / `spawn_batch`: spawn N children + join per
//!      `JoinPolicy::{All, Any, Abandon}`.
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - Order of operations above is fixed and asserted by
//!   `tests/depth_exceeded_before_quota.rs`.
//! - Per-parent lease atomicity: a single in-flight spawn-decision
//!   per parent at a time. Audit sequence numbers are strictly
//!   monotone with no gaps under concurrent parent-side spawn calls.
//! - Cross-parent parallelism: unrelated parents own DISTINCT
//!   `Arc<Mutex<()>>` lease handles.
//! - Kernel-only audit writes: every `SubagentSpawn` audit record
//!   goes through `aura_agent_kernel::write_system_record`.
//! - Detached child cancellation: the parent's `CancellationToken`
//!   does NOT propagate; the fleet shutdown token always does.
//! - Idempotency: identical `(parent_agent_id, tool_call_id)` within
//!   the dedupe window short-circuits to the cached outcome —
//!   producing zero additional children + zero additional audit
//!   records. Asserted by `tests/spawn_idempotent_dedupe.rs`.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod handle;
mod lease;
mod orphan;
mod runner;
mod spawner;

pub use handle::{BatchOutcome, BatchSpawn, DetachedSpawn, SpawnHandle};
pub use lease::{DedupedSpawn, ParentLease, ParentLeaseRegistry, DEFAULT_DEDUPE_WINDOW};
pub use orphan::{OrphanRecord, OrphanStore, OrphanStoreError};
pub use runner::{ChildRunContext, ChildRunError, ChildRunner};
pub use spawner::{
    promote_to_orphan, FleetSpawner, FleetSpawnerConfig, SpawnError, SpawnRequest,
    SubagentSpawnRecordPayload, RECORD_KIND_SUBAGENT_SPAWN,
};

// Re-export the enums callers commonly need so they don't have to
// pull aura-core-modes / aura-agent-subagent transitively.
pub use aura_agent_subagent::{DerivationError, ParentContext, SubagentOverrides, SubagentSpec};
pub use aura_core_modes::{JoinPolicy, ModeViolation, SpawnMode};
