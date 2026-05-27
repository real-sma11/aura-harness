//! Automaton-builtin knobs (`spec_gen` / `task_refinement` token caps
//! and the dev-loop retry-note truncation budget).

// ---------------------------------------------------------------------------
// Compile-time constants
// ---------------------------------------------------------------------------

/// `max_tokens` cap on the auxiliary LLM call used by the task
/// refinement automaton.
pub const REFINEMENT_MAX_TOKENS: u32 = 4_096;

/// `max_tokens` cap on the auxiliary LLM call used by the spec
/// generation automaton.
pub const SPEC_GEN_MAX_TOKENS: u32 = 32_768;

/// Maximum bytes of `build_retry_note` injected back into the next
/// task attempt by the dev-loop automaton. Larger notes are
/// head/tail-truncated so the bootstrap user message stays within the
/// Cloudflare body cap.
pub const DEV_LOOP_RETRY_NOTE_MAX_BYTES: usize = 12_000;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Automaton-layer config (all compile-time today).
#[derive(Debug, Clone, Copy)]
pub struct AutomatonConfig {
    /// See [`REFINEMENT_MAX_TOKENS`].
    pub refinement_max_tokens: u32,
    /// See [`SPEC_GEN_MAX_TOKENS`].
    pub spec_gen_max_tokens: u32,
    /// See [`DEV_LOOP_RETRY_NOTE_MAX_BYTES`].
    pub dev_loop_retry_note_max_bytes: usize,
}

impl AutomatonConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            refinement_max_tokens: REFINEMENT_MAX_TOKENS,
            spec_gen_max_tokens: SPEC_GEN_MAX_TOKENS,
            dev_loop_retry_note_max_bytes: DEV_LOOP_RETRY_NOTE_MAX_BYTES,
        }
    }
}

impl Default for AutomatonConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
