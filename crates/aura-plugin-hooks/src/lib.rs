//! # aura-plugin-hooks
//!
//! Layer: plugin
//!
//! Lifecycle hook engine for first-party Aura plugins. Phase 4c shipped
//! the [`HookEvent`] taxonomy + [`HookEngine`] shell. **Phase 8** wires
//! firing for real at every lifecycle point in the agent loop / fleet
//! spawner.
//!
//! ## Surfaces
//!
//! - [`HookEvent`] — closed enum of the 10 Codex / Claude lifecycle
//!   events plugins can subscribe to.
//! - [`ctx`] — per-event ctx structs (`SessionStartHookCtx`,
//!   `PreToolUseHookCtx`, etc.) handed to [`HookEngine::fire_event`].
//! - [`HookEngine`] — registration + Phase 8 firing surface. The
//!   `is_empty(event)` gate guarantees the empty-install backward-
//!   compat invariant: zero overhead beyond a single hashmap lookup.
//! - [`HookOutcome`] / [`AggregateOutcome`] — handler return values
//!   and aggregate firing decisions.
//! - [`Redacted`] — `Debug`-redacting wrapper for sensitive ctx
//!   fields (tool args, prompt text, …).
//! - [`sandbox`] — env-var scrubbing (`SECRET_PATTERNS` deny list)
//!   + canonical Aura / Codex / Claude alias env injection.
//! - Error types: [`HookError`], [`HookEngineError`], [`HookFireError`].
//!
//! ## Invariants ([rules.md §13])
//!
//! - Spawned hooks see an explicitly scrubbed env (cloud-provider
//!   credentials, `*_TOKEN`, `*_KEY`, `*_SECRET`, `*_PASSWORD`,
//!   `KUBECONFIG`, `SSH_AUTH_SOCK`, etc. are stripped) plus the
//!   canonical Aura vars + Codex / Claude compat aliases. See
//!   [`sandbox::SECRET_PATTERNS`] / [`sandbox::SECRET_NAMES`].
//! - Hook command resolution: a bare filename (no `/` or `\`) is
//!   passed verbatim so the OS PATH lookup applies; a relative path
//!   WITH a separator joins to `plugin_root`; an absolute path is
//!   used as-is.
//! - Per-handler 5-second wall-clock timeout. Overruns produce
//!   [`HookOutcome::TimedOut`] and the engine continues with the
//!   remaining handlers.
//! - [`HookEvent`] is a **closed enum**. Adding a variant breaks
//!   compilation for every match site (intentional).
//! - [`HookOutcome`] is a **closed enum**. Adding a variant breaks
//!   compilation for every aggregation site (intentional).

#![forbid(unsafe_code)]
#![warn(clippy::all)]

pub mod context;
pub mod ctx;
pub mod engine;
pub mod error;
pub mod event;
pub mod host;
pub mod outcome;
pub mod redacted;
pub mod sandbox;

pub use context::HookFiringContext;
pub use ctx::{
    CtxMeta, HookCtx, PermissionRequestHookCtx, PluginLoadFailure, PluginRef, PostCompactHookCtx,
    PostToolUseHookCtx, PreCompactHookCtx, PreToolUseHookCtx, SessionStartHookCtx, StopHookCtx,
    SubagentStartHookCtx, SubagentStopHookCtx, UserPromptSubmitHookCtx,
};
pub use engine::{
    make_meta, HookEngine, HookFireSummary, RegisteredHook, SharedHookEngine, DEFAULT_HOOK_TIMEOUT,
    HOOK_STDOUT_CAP_BYTES,
};
pub use error::{HookEngineError, HookError};
pub use event::HookEvent;
pub use host::{is_blocked, PluginHookHost};
pub use outcome::{AggregateOutcome, HookFireError, HookOutcome};
pub use redacted::Redacted;
