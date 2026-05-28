//! Steering shim — re-exports the agent-steering crate.
//!
//! Phase 6a moved every stateful evaluator (`RepeatedReadTracker`,
//! `ImplementNowSteering`, `EarlyTestOracle`) plus the
//! `TurnSteering` trait and `SteeringRegistry` into the dedicated
//! `aura-agent-steering` crate so the steering layer sits strictly
//! below the agent loop in the layer order.
//!
//! This module preserves the historical
//! `crate::agent_loop::steering::*` import paths so existing call
//! sites inside `aura-agent` (and downstream tests) keep working
//! without churn. New code should import from `aura_agent_steering`
//! directly.

// Re-export the full evaluator + trait surface so existing
// `crate::agent_loop::steering::*` import paths inside `aura-agent`
// and downstream tests keep resolving without churn. Some of these
// names are only used by external (downstream) consumers; suppress
// the in-crate "unused re-export" warning since the whole point of
// this module is to expose them.
#[allow(unused_imports)]
pub use aura_agent_steering::{
    inject, EarlyTestOracle, ImplementNowSteering, RepeatedReadTracker, SteeringRegistry,
    TurnSteering,
};
