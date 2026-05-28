//! # aura-plugin-hooks
//!
//! Layer: plugin
//!
//! Lifecycle hook engine for first-party Aura plugins. Phase 4c
//! deliverable: ships the [`HookEvent`] taxonomy, the [`HookEngine`]
//! shell, and the [`HookFiringContext`] env-injection contract. The
//! engine can fire registered hooks via manual driver code (see the
//! `hook_event_smoke` integration test); no caller in the agent loop
//! fires events yet — that wiring lands in Phase 8.
//!
//! ## Surfaces
//!
//! - [`HookEvent`] — closed enum of the 10 Codex/Claude lifecycle
//!   events plugins can subscribe to. Adding a variant is a breaking
//!   change for plugin authors; the engine validates events at load
//!   time against this enum.
//! - [`HookFiringContext`] — per-firing context handed to a hook
//!   process. Carries the plugin root, event, agent / session / turn
//!   identifiers, and a free-form `extra` map. The
//!   [`HookFiringContext::env_vars`] helper builds the env var set
//!   injected into the spawned hook process, including the
//!   `CODEX_PLUGIN_ROOT` / `CLAUDE_PLUGIN_ROOT` compatibility aliases.
//! - [`HookEngine`] — registration + firing surface. Hooks register
//!   per [`HookEvent`] and fire in registration order. Process-level
//!   failures are isolated — a failed hook does not block the
//!   remaining registered hooks for the same event.
//! - [`sandbox`] — env-var scrubbing for hook subprocesses. Plugin
//!   authors must NOT inherit operator cloud / model credentials; the
//!   sandbox starts with an empty env and re-adds only the minimal
//!   PATH / HOME / locale set.
//! - Error types: [`HookError`], [`HookEngineError`].
//!
//! ## Invariants ([rules.md §13])
//!
//! - Spawned hooks see an **explicitly scrubbed** env: the
//!   sandbox-allowed inheritance list ([`sandbox::SAFE_INHERIT`])
//!   plus the [`HookFiringContext::env_vars`] payload plus any
//!   plugin-author-allowlisted `env` overrides from the registered
//!   hook. Operator secrets such as `AWS_*`, `ANTHROPIC_*`,
//!   `OPENAI_*`, `GITHUB_TOKEN` are NOT inherited.
//! - Hook command resolution: a bare filename (no `/` or `\`) is
//!   passed verbatim so the OS PATH lookup applies; a relative path
//!   WITH a separator joins to `plugin_root`; an absolute path is
//!   used as-is. See [`engine::HookEngine::spawn_one`] for the
//!   implementation.
//! - [`HookEvent`] is a **closed enum**. The
//!   `closed_enum_invariant` test in [`event`] matches every variant
//!   without a `_` wildcard so adding a variant breaks compilation
//!   (intentional).

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod context;
pub mod engine;
pub mod error;
pub mod event;
pub mod sandbox;

pub use context::HookFiringContext;
pub use engine::{HookEngine, HookFireSummary, RegisteredHook};
pub use error::{HookEngineError, HookError};
pub use event::HookEvent;
