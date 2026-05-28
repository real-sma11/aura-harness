//! Error types for the hook engine.

use thiserror::Error;

use crate::event::HookEvent;

/// Reasons a single hook subprocess can fail. Surfaced to the engine
/// driver so a failed hook is observable without aborting the firing
/// loop for the remaining hooks.
#[derive(Debug, Error)]
pub enum HookError {
    /// The hook process failed to spawn (binary missing, permission
    /// denied, etc.).
    #[error("hook spawn failed for plugin `{plugin_id}` event {event:?}: {source}")]
    Spawn {
        /// Plugin id that owns the failing hook.
        plugin_id: String,
        /// Event being fired when the spawn failed.
        event: HookEvent,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The hook process spawned but exited non-zero.
    #[error("hook exited non-zero for plugin `{plugin_id}` event {event:?}: code={code:?}")]
    ExitFailure {
        /// Plugin id that owns the failing hook.
        plugin_id: String,
        /// Event being fired when the failure happened.
        event: HookEvent,
        /// Exit code (None on Unix when the process was killed by a
        /// signal).
        code: Option<i32>,
    },
}

/// Higher-level engine-driver errors. Distinct from [`HookError`] so
/// the firing loop can surface per-hook failures separately from
/// engine-level invariant violations.
#[derive(Debug, Error)]
pub enum HookEngineError {
    /// Manifest declared an event name the engine does not recognise
    /// (i.e. not one of the 10 [`HookEvent`] variants). Phase 4c
    /// surfaces this as a warning at load time; Phase 8 fires it.
    #[error("unknown hook event: {event}")]
    UnknownEvent {
        /// The unrecognised event string as it appeared on disk.
        event: String,
    },
}
