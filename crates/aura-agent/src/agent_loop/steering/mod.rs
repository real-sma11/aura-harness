//! Stateful steering evaluators owned by the agent loop.
//!
//! Phase 2 of the core-loop architecture refactor split the steering
//! subsystem in two:
//!
//! - `aura-prompts/src/steering/` owns the **render** half — the
//!   `SteeringKind` enum, per-variant body text, and the
//!   `<harness_steering>` envelope wrapper. That crate has no
//!   reasoner dep and no agent-loop state on its surface.
//! - This module (the **evaluation + injection** half) owns the
//!   stateful evaluators that translate live agent-loop signals into
//!   `SteeringKind` values, plus the `inject` helper that appends
//!   the rendered envelope to `Vec<aura_reasoner::Message>`.
//!
//! Phase 5 introduces the [`TurnSteering`] trait and
//! [`SteeringRegistry`] that own every evaluator uniformly. Each
//! `TurnSteering` source carries its own per-turn state and exposes
//! three hooks the loop drives every turn:
//!
//! - [`TurnSteering::observe_tool`] — called once per `(tool, result)`
//!   pair on every transport path (buffered + pump) via
//!   `SteeringRegistry::observe_tool` from
//!   [`crate::agent_loop::tool_pipeline::track_tool_effects`].
//! - [`TurnSteering::begin_turn`] — called once at the top of every
//!   iteration from `LoopState::begin_iteration` before the next
//!   request is built. Sources reset per-turn counters or arm
//!   pending nudges here.
//! - [`TurnSteering::drain_for_next_turn`] — drained once per turn
//!   after `begin_turn` to collect every [`aura_prompts::SteeringKind`]
//!   that should be injected via [`inject::inject`] into the
//!   message stream before sampling.
//!
//! Sources installed today (see [`SteeringRegistry::for_config`]):
//!
//! - [`repeated_read::RepeatedReadTracker`] — three-or-more identical
//!   reads in a turn enqueue a [`aura_prompts::SteeringKind::RepeatedRead`]
//!   nudge for the next turn.
//! - [`implement_now::ImplementNowSteering`] — once exploration calls
//!   cross the configured threshold without any cumulative file
//!   write, enqueue a one-shot
//!   [`aura_prompts::SteeringKind::ImplementNow`]. The source tracks
//!   its own exploration / write counters via [`TurnSteering::observe_tool`]
//!   so the gate is self-contained (no shared state with `LoopState`).
//! - [`early_oracle::EarlyTestOracle`] — when a `TaskRun` task opts
//!   into the early test-gate oracle and the first read-only batch
//!   closes (or a write tool fires before any read), enqueue a
//!   single [`aura_prompts::SteeringKind::TaskAlreadySatisfiedHint`].
//!
//! The relocation also fixes the previous layer violation
//! (`prompts/steering/implement_now_gate.rs` reaching into
//! `agent_loop::{LoopState, AgentLoopConfig}` from a sibling
//! "prompts" module): every evaluator now lives in the same crate /
//! module tree as the state it inspects.

pub mod early_oracle;
pub mod implement_now;
pub mod inject;
pub mod repeated_read;

use aura_prompts::SteeringKind;

pub(crate) use early_oracle::EarlyTestOracle;
pub use inject::inject;
pub use repeated_read::RepeatedReadTracker;

use crate::types::{ToolCallInfo, ToolCallResult};

use super::config::AgentLoopConfig;

/// Per-turn steering source.
///
/// Every evaluator is driven by the same three hooks regardless of
/// whether it is gating exploration, surfacing repeated-read nudges,
/// or arming a one-shot test-gate hint. The registry calls them in
/// this order:
///
/// 1. [`Self::observe_tool`] — invoked from
///    `tool_pipeline::track_tool_effects` for each
///    `(ToolCallInfo, ToolCallResult)` pair the dispatched batch
///    produced, on both the buffered and pump transports. Sources
///    update internal counters / state machines here.
/// 2. [`Self::begin_turn`] — invoked from
///    `LoopState::begin_iteration` once per iteration before the
///    next request is built. Sources reset per-turn counters or
///    arm pending nudges here.
/// 3. [`Self::drain_for_next_turn`] — invoked immediately after
///    `begin_turn` so the loop can route the returned
///    [`SteeringKind`]s through [`inject::inject`].
///
/// Sources must be `Send`: the registry is stored on `LoopState`
/// which is moved across `.await` points inside the agent loop.
pub(crate) trait TurnSteering: Send {
    /// Observe one `(tool, result)` pair. Implementations update
    /// internal state but do NOT inject anything here — injection
    /// happens once per turn through [`Self::drain_for_next_turn`].
    fn observe_tool(&mut self, tool: &ToolCallInfo, result: &ToolCallResult);

    /// Called once per turn from `LoopState::begin_iteration` before
    /// the next request is built. Sources typically reset per-turn
    /// counters here (e.g. [`RepeatedReadTracker`] zeroes its
    /// `(content_hash → count)` map) so the next turn's accounting
    /// starts clean. Sources may also queue nudges here that depend
    /// on cumulative state across multiple turns (e.g.
    /// [`implement_now::ImplementNowSteering`] arms its one-shot
    /// nudge here once its internal exploration counter crosses the
    /// threshold).
    fn begin_turn(&mut self);

    /// Drain whatever [`SteeringKind`]s should be injected before
    /// the next model request. Called once per turn after
    /// [`Self::begin_turn`]. Sources return either an empty vec or
    /// the nudges queued by prior `observe_tool` / `begin_turn`
    /// calls.
    fn drain_for_next_turn(&mut self) -> Vec<SteeringKind>;
}

/// Owner of every [`TurnSteering`] source for one `LoopState`.
///
/// The registry exposes one method per `TurnSteering` hook plus a
/// `push` helper used by [`Self::for_config`]. Production code
/// never touches individual sources by type — every per-turn
/// mutation flows through `observe_tool` / `begin_turn` /
/// `drain_for_next_turn` so adding or removing a source is a
/// one-line change at the install site.
///
/// # Why every source is self-contained
///
/// Phase 5 deliberately made every `TurnSteering` source own all
/// of the state it needs:
///
/// - [`RepeatedReadTracker`] keeps its `(content_hash → count)` map
///   and queued nudges.
/// - [`implement_now::ImplementNowSteering`] tracks its own
///   exploration counter, write latch, and recent-read paths via
///   `observe_tool` instead of reading them off `LoopState`.
/// - [`EarlyTestOracle`] owns its `AwaitingFirstRead →
///   InsideFirstBatch → HintQueued → Done` state machine.
///
/// `LoopState` still maintains a parallel `had_any_file_write` /
/// `implement_now_injected` pair because the pre-dispatch circling
/// gate in `tool_pipeline::partition_circling_duplicate_reads`
/// consults them directly. The registry forwards the
/// `implement-now-was-drained` event back to the loop via
/// [`Self::implement_now_injected`] so the two stay in lockstep
/// without sharing mutable state.
pub(crate) struct SteeringRegistry {
    sources: Vec<Box<dyn TurnSteering>>,
    /// Once-per-run latch armed by `drain_for_next_turn` whenever a
    /// [`SteeringKind::ImplementNow`] was drained. Read by
    /// `LoopState::begin_iteration` so the
    /// `partition_circling_duplicate_reads` gate keeps firing
    /// through subsequent iterations.
    implement_now_injected: bool,
}

impl SteeringRegistry {
    /// Construct an empty registry. Production callers use
    /// [`Self::for_config`] instead so the default source set is
    /// installed.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            sources: Vec::new(),
            implement_now_injected: false,
        }
    }

    /// Install the standard steering sources for the given
    /// [`AgentLoopConfig`]:
    ///
    /// - [`RepeatedReadTracker`] (always installed).
    /// - [`implement_now::ImplementNowSteering`] (always installed;
    ///   short-circuits internally when
    ///   `aura_config::agent().steering.implement_now_enabled` is
    ///   `false` or when the caller has no
    ///   [`AgentLoopConfig::phase_reset_signal`]).
    /// - [`EarlyTestOracle`] (installed only when
    ///   [`AgentLoopConfig::early_test_oracle`] is
    ///   `Some(EarlyTestOracleConfig { enabled: true, .. })`).
    #[must_use]
    pub(crate) fn for_config(config: &AgentLoopConfig) -> Self {
        let mut registry = Self::new();
        registry.push(Box::new(RepeatedReadTracker::new()));
        registry.push(Box::new(implement_now::ImplementNowSteering::new(
            config.phase_reset_signal.is_some(),
        )));
        if let Some(oracle_cfg) = config.early_test_oracle.as_ref() {
            if oracle_cfg.enabled {
                registry.push(Box::new(EarlyTestOracle::new(
                    oracle_cfg.test_command.clone(),
                    true,
                )));
            }
        }
        registry
    }

    /// Append `source` to the back of the registry. New sources are
    /// observed / drained in install order.
    pub(crate) fn push(&mut self, source: Box<dyn TurnSteering>) {
        self.sources.push(source);
    }

    /// Forward `(tool, result)` to every installed source. Called
    /// from `tool_pipeline::track_tool_effects` for each pair the
    /// dispatched batch produced so both transports observe
    /// identical updates.
    pub(crate) fn observe_tool(&mut self, tool: &ToolCallInfo, result: &ToolCallResult) {
        for source in &mut self.sources {
            source.observe_tool(tool, result);
        }
    }

    /// Called from `LoopState::begin_iteration` once per turn.
    pub(crate) fn begin_turn(&mut self) {
        for source in &mut self.sources {
            source.begin_turn();
        }
    }

    /// Drain every queued [`SteeringKind`] across every installed
    /// source. Called once per turn after [`Self::begin_turn`].
    pub(crate) fn drain_for_next_turn(&mut self) -> Vec<SteeringKind> {
        let mut out = Vec::new();
        for source in &mut self.sources {
            let drained = source.drain_for_next_turn();
            for kind in &drained {
                if matches!(kind, SteeringKind::ImplementNow { .. }) {
                    self.implement_now_injected = true;
                }
            }
            out.extend(drained);
        }
        out
    }

    /// Returns `true` once the implement-now source has drained a
    /// [`SteeringKind::ImplementNow`] for this run. Read by
    /// `LoopState::begin_iteration` to keep `state.implement_now_injected`
    /// in lockstep, which in turn drives the pre-dispatch
    /// circling-read gate in
    /// `tool_pipeline::partition_circling_duplicate_reads`.
    #[must_use]
    pub(crate) fn implement_now_injected(&self) -> bool {
        self.implement_now_injected
    }
}

impl Default for SteeringRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// Phase-1 back-compat aliases (`REPEATED_READ_THRESHOLD`,
// `IMPLEMENT_NOW_DEFAULT_THRESHOLD`) are intentionally NOT re-exported
// here. Phase 2 hard-deletes them; consumers read the values directly
// from `aura_config::*`.

#[cfg(test)]
mod registry_tests;
