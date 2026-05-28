//! # aura-reasoner (compatibility shell)
//!
//! Phase 3: this crate's implementation moved to `aura-model-reasoner`.
//! Re-exported here so existing call sites keep compiling without a
//! flag-day rename. New code should depend on `aura-model-reasoner`
//! directly.
//!
//! Layer: model (shell)

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub use aura_model_reasoner::*;
