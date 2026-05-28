//! [`TurnSteering`] trait + [`SteeringRegistry`].
//!
//! Relocated from `aura-agent::agent_loop::steering::mod` in
//! Phase 6a. The trait and registry are unchanged on the wire — the
//! only public-API delta is that `SteeringRegistry::for_config` no
//! longer takes a borrow on `AgentLoopConfig` (which lives in
//! `aura-agent`); callers in `aura-agent` now spread the two
//! relevant fields explicitly via [`SteeringRegistry::for_loop`].

use aura_config::EarlyTestOracleConfig;
use aura_prompts::SteeringKind;

use crate::early_oracle::EarlyTestOracle;
use crate::implement_now::ImplementNowSteering;
use crate::repeated_read::RepeatedReadTracker;
use crate::types::{ToolCallInfo, ToolCallResult};

/// Per-turn steering source.
///
/// Every evaluator is driven by the same three hooks regardless of
/// whether it is gating exploration, surfacing repeated-read nudges,
/// or arming a one-shot test-gate hint. The registry calls them in
/// this order:
///
/// 1. [`Self::observe_tool`] — invoked from the agent loop's
///    `tool_pipeline::track_tool_effects` for each
///    `(ToolCallInfo, ToolCallResult)` pair the dispatched batch
///    produced, on both the buffered and pump transports. Sources
///    update internal counters / state machines here.
/// 2. [`Self::begin_turn`] — invoked from
///    `LoopState::begin_iteration` once per iteration before the
///    next request is built. Sources reset per-turn counters or arm
///    pending nudges here.
/// 3. [`Self::drain_for_next_turn`] — invoked immediately after
///    `begin_turn` so the loop can route the returned
///    [`SteeringKind`]s through [`crate::inject::inject`].
///
/// Sources must be `Send`: the registry is stored on `LoopState`
/// which is moved across `.await` points inside the agent loop.
pub trait TurnSteering: Send {
    /// Observe one `(tool, result)` pair. Implementations update
    /// internal state but do NOT inject anything here — injection
    /// happens once per turn through
    /// [`Self::drain_for_next_turn`].
    fn observe_tool(&mut self, tool: &ToolCallInfo, result: &ToolCallResult);

    /// Called once per turn from `LoopState::begin_iteration` before
    /// the next request is built. Sources typically reset per-turn
    /// counters here (e.g. [`RepeatedReadTracker`] zeroes its
    /// `(content_hash → count)` map) so the next turn's accounting
    /// starts clean. Sources may also queue nudges here that depend
    /// on cumulative state across multiple turns (e.g.
    /// [`ImplementNowSteering`] arms its one-shot nudge here once
    /// its internal exploration counter crosses the threshold).
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
/// `push` helper used by [`Self::for_loop`]. Production code never
/// touches individual sources by type — every per-turn mutation
/// flows through `observe_tool` / `begin_turn` /
/// `drain_for_next_turn` so adding or removing a source is a
/// one-line change at the install site.
///
/// # Why every source is self-contained
///
/// Phase 5 deliberately made every `TurnSteering` source own all of
/// the state it needs:
///
/// - [`RepeatedReadTracker`] keeps its `(content_hash → count)` map
///   and queued nudges.
/// - [`ImplementNowSteering`] tracks its own exploration counter,
///   write latch, and recent-read paths via `observe_tool` instead
///   of reading them off `LoopState`.
/// - [`EarlyTestOracle`] owns its `AwaitingFirstRead →
///   InsideFirstBatch → HintQueued → Done` state machine.
///
/// The agent loop's `LoopState` still maintains a parallel
/// `had_any_file_write` / `implement_now_injected` pair because the
/// pre-dispatch circling gate in
/// `tool_pipeline::partition_circling_duplicate_reads` consults
/// them directly. The registry forwards the
/// `implement-now-was-drained` event back to the loop via
/// [`Self::implement_now_injected`] so the two stay in lockstep
/// without sharing mutable state.
pub struct SteeringRegistry {
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
    /// [`Self::for_loop`] instead so the default source set is
    /// installed.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            implement_now_injected: false,
        }
    }

    /// Install the standard steering sources for an agent loop:
    ///
    /// - [`RepeatedReadTracker`] (always installed).
    /// - [`ImplementNowSteering`] (always installed;
    ///   short-circuits internally when
    ///   `aura_config::agent().steering.implement_now_enabled` is
    ///   `false` or when the caller has no phase-reset signal).
    /// - [`EarlyTestOracle`] (installed only when
    ///   `early_test_oracle` is
    ///   `Some(EarlyTestOracleConfig { enabled: true, .. })`).
    ///
    /// Phase 6a renamed this from `for_config(&AgentLoopConfig)` to
    /// `for_loop(bool, Option<EarlyTestOracleConfig>)` so the
    /// steering crate no longer needs an `aura-agent` dep just to
    /// borrow two fields off `AgentLoopConfig`. `aura-agent`
    /// constructs the registry in `LoopState::new` using
    /// `config.phase_reset_signal.is_some()` and
    /// `config.early_test_oracle.clone()`.
    #[must_use]
    pub fn for_loop(
        phase_reset_signal_present: bool,
        early_test_oracle: Option<EarlyTestOracleConfig>,
    ) -> Self {
        let mut registry = Self::new();
        registry.push(Box::new(RepeatedReadTracker::new()));
        registry.push(Box::new(ImplementNowSteering::new(
            phase_reset_signal_present,
        )));
        if let Some(oracle_cfg) = early_test_oracle {
            if oracle_cfg.enabled {
                registry.push(Box::new(EarlyTestOracle::new(
                    oracle_cfg.test_command,
                    true,
                )));
            }
        }
        registry
    }

    /// Append `source` to the back of the registry. New sources are
    /// observed / drained in install order.
    pub fn push(&mut self, source: Box<dyn TurnSteering>) {
        self.sources.push(source);
    }

    /// Forward `(tool, result)` to every installed source. Called
    /// from `tool_pipeline::track_tool_effects` for each pair the
    /// dispatched batch produced so both transports observe
    /// identical updates.
    pub fn observe_tool(&mut self, tool: &ToolCallInfo, result: &ToolCallResult) {
        for source in &mut self.sources {
            source.observe_tool(tool, result);
        }
    }

    /// Called from `LoopState::begin_iteration` once per turn.
    pub fn begin_turn(&mut self) {
        for source in &mut self.sources {
            source.begin_turn();
        }
    }

    /// Drain every queued [`SteeringKind`] across every installed
    /// source. Called once per turn after [`Self::begin_turn`].
    pub fn drain_for_next_turn(&mut self) -> Vec<SteeringKind> {
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
    /// `LoopState::begin_iteration` to keep
    /// `state.implement_now_injected` in lockstep, which in turn
    /// drives the pre-dispatch circling-read gate in
    /// `tool_pipeline::partition_circling_duplicate_reads`.
    #[must_use]
    pub fn implement_now_injected(&self) -> bool {
        self.implement_now_injected
    }
}

impl Default for SteeringRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod registry_tests;
