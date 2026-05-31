//! # aura-exec-traits
//!
//! Layer: exec
//!
//! Low-level executor and spawn-hook traits for the exec layer.
//!
//! These traits were relocated out of the agent-layer `aura-agent-kernel`
//! crate so that the exec-layer `aura-tools` crate can consume them without
//! an upward layer dependency. The crate depends only on `aura-core` (plus
//! non-aura deps) so both `aura-tools` and `aura-agent-kernel` can depend on
//! it downward without forming a cycle.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

mod executor;
mod router;
mod spawn_hook;

pub use executor::{
    decode_tool_effect, DecodedToolResult, ExecuteContext, ExecuteLimits, Executor, ExecutorError,
};
pub use router::ExecutorRouter;
pub use spawn_hook::{ChildAgentSpec, NoopSpawnHook, SpawnError, SpawnHook, SpawnOutcome};
