//! `RecordKind` — closed taxonomy of record-log entry kinds with a
//! forward-compat fallback variant.
//!
//! The variant list reflects the architecture plan §4 high-level
//! taxonomy of effects the audited kernel records: model proposals,
//! tool effects, child spawns, compaction events, steering
//! decisions, policy verdicts, and permission updates. Phase 6a will
//! consume `PermissionsUpdate` to absorb today's `aura-runtime`
//! direct-append site.
//!
//! ## Forward-compatibility invariant
//!
//! [`RecordKind::Unknown`] uses serde's `#[serde(other)]` fallback,
//! which serde only supports for unit variants. The plan called for
//! `Unknown(u32)` to carry a numeric tag, but a struct/tuple variant
//! is incompatible with `#[serde(other)]` today. The deviation:
//!
//! - The variant itself is unit (`Unknown`).
//! - A constructor [`RecordKind::unknown_with_id`] preserves the
//!   future-extension shape so callers that gain access to a numeric
//!   tag (e.g. a future binary wire format) can pass it through; the
//!   id is intentionally discarded for V1 because we have no field
//!   to store it.
//!
//! When deserialising JSON like `{"kind": "totally_made_up"}` an old
//! `aura-node` reading new records observes [`RecordKind::Unknown`]
//! instead of crashing on an unknown variant.

use serde::{Deserialize, Serialize};

/// Closed taxonomy of record-log entry kinds plus a forward-compat
/// `Unknown` fallback.
///
/// Wire format: `#[serde(tag = "kind", rename_all = "snake_case")]`
/// so e.g. `RecordKind::ModelProposal` round-trips through
/// `{"kind": "model_proposal"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordKind {
    /// Reasoner produced a [`crate::RecordEntry`]-shape proposal that
    /// the kernel will adjudicate.
    ModelProposal,
    /// Kernel committed a tool effect (executor result).
    ToolEffect,
    /// Parent agent spawned a child agent.
    SpawnChild,
    /// Compaction event collapsed a window of prior records.
    Compaction,
    /// Steering layer made a routing decision.
    SteeringDecision,
    /// Policy gate emitted an approve/deny/ask verdict.
    PolicyVerdict,
    /// Per-tool / per-agent permission ledger update — the entry
    /// Phase 6a uses to consolidate today's `aura-runtime` direct
    /// append into the audited kernel write path.
    PermissionsUpdate,
    /// Phase 7b: parent agent successfully derived + dispatched a
    /// child subagent. Payload carries the `OverrideManifest` and
    /// child agent id so an auditor can reconstruct parent intent.
    SubagentSpawn,
    /// Phase 7b: child agent died because its parent's task future
    /// was dropped (panic / cancellation / natural completion) AND
    /// the child had been spawned in `SpawnMode::Wait`. Wait-mode
    /// children are tied to the parent's stack frame; this is the
    /// audit record stamped when the parent goes away.
    ChildCancelledByParentDeath,
    /// Phase 7b: child agent was promoted to an orphan because its
    /// parent's task future was dropped AND the child had been
    /// spawned in `SpawnMode::Detached` (or `SpawnMode::Batch` with
    /// `JoinPolicy::Abandon`). The orphan continues running and its
    /// state is persisted under `~/.aura/state/orphans/<id>.json`.
    ChildOrphanedByParentDeath,
    /// Phase 7b: an orphan was reaped via `aura agents reap`. The
    /// payload carries the agent id, the reap source (cli /
    /// shutdown), and the wall-clock cancellation result.
    OrphanReaped,
    /// Phase 10: session lifecycle end marker. Emitted by the
    /// `FleetDaemon` shutdown path with the cumulative session
    /// telemetry (iteration count, token totals, wall-clock
    /// duration) and a `clean_shutdown` flag that distinguishes
    /// graceful drains from grace-period timeouts.
    ///
    /// The on-wire payload shape (`SessionStopRecordPayload`) lives
    /// in `aura_fleet_daemon` so the store crate stays free of any
    /// upward dep on the fleet layer.
    SessionStop,
    /// Phase 10: a tool call was vetoed mid-flight by a registered
    /// [`HookEvent::PreToolUse`] handler returning
    /// [`HookOutcome::Block`]. The audit row replaces the normal
    /// `ToolCallResult` row that would otherwise be written, so an
    /// auditor can still observe the tool call attempt and the
    /// block reason without surfacing it as an executor failure.
    ToolCallBlockedByHook,

    /// Forward-compatibility fallback. Old `aura-node` reading newer
    /// records emits [`RecordKind::Unknown`] instead of failing
    /// deserialisation when the wire `kind` tag does not match any
    /// known variant.
    ///
    /// Constructed via [`RecordKind::unknown_with_id`] when a future
    /// binary tag is available; otherwise serde's `#[serde(other)]`
    /// fallback fills it in automatically.
    #[serde(other)]
    Unknown,
}

impl RecordKind {
    /// Construct a [`RecordKind::Unknown`] from a numeric id.
    ///
    /// Phase 2 placeholder: the id is currently discarded because
    /// the JSON wire format we ship today does not carry numeric
    /// kind tags. The constructor exists so a future binary wire
    /// format (or a downgraded `aura-node` consuming a newer record
    /// stream) has a stable API to call when it observes a tag it
    /// does not recognise.
    #[must_use]
    pub const fn unknown_with_id(_id: u32) -> Self {
        Self::Unknown
    }

    /// Convenience predicate for the forward-compat fallback variant.
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}
