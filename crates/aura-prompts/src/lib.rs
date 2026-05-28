//! # aura-prompts (compatibility shell)
//!
//! Phase 3: this crate's implementation moved to `aura-context-prompts`.
//! Re-exported here so existing call sites keep compiling without a
//! flag-day rename. New code should depend on `aura-context-prompts`
//! directly.
//!
//! Layer: context (shell)

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_context_prompts::*;
