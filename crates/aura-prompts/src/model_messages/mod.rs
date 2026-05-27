//! Standalone model-facing strings the harness sends back to the
//! model through the tool-result channel (or as side-message text).
//!
//! These do not flow through the [`crate::steering::SteeringRenderer`]
//! envelope — they are not per-iteration "the harness is steering
//! you" announcements; they are concrete error / warning / status
//! messages tied to a specific tool result. Each submodule owns one
//! conceptual string family. The submodule layout matches the plan's
//! `model_messages/{chunk_guard, implement_now, max_tokens, task_done,
//! auto_build, test_warning}` decomposition.
//!
//! Each callable here either returns a `&'static str` (when the body
//! has no runtime inputs) or a `String` produced by a small
//! formatter. Consumers read through here so the
//! [`tests/prompts_boundary.rs`] guardrail can hold the line "all
//! model-facing strings under `aura-prompts/`".

pub mod auto_build;
pub mod chunk_guard;
pub mod implement_now;
pub mod max_tokens;
pub mod task_done;
pub mod test_warning;
