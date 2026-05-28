//! Policy engine for authorizing proposals and tool usage.
//!
//! ## Tool States
//!
//! Per-tool enablement resolves through
//! [`aura_core::resolve_effective_permission`] into
//! [`aura_core::ToolState`] (`on` / `off` / `ask`). Capability, scope,
//! integration, and execution guardrails remain separate policy layers.
//!
//! The module is split into:
//! - [`config`] — shape types (`PolicyConfig`).
//! - [`check`] — the [`Policy`] engine itself plus its authorization
//!   pipeline (`check`, `check_with_runtime_capabilities`,
//!   agent-permission + runtime-capability checks).
//!
//! The public API is re-exported from both submodules so downstream
//! crates still import via `aura_kernel::policy::{Policy, PolicyConfig,
//! PolicyResult}`.

mod check;
mod config;

pub use check::{Policy, PolicyResult, PolicyVerdict};
pub use config::PolicyConfig;
