//! Prompt-layer knobs (bootstrap budget, repeated-read display chars,
//! compaction-summary block cap).

use crate::env::{
    lookup_bool, lookup_numeric, AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS,
    AURA_AGENT_BOOTSTRAP_SPEC_BYTES, AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES, TRUTHY_LITERALS,
};

// ---------------------------------------------------------------------------
// Compile-time constants
// ---------------------------------------------------------------------------

/// Default cap on `spec.markdown_contents` bytes injected into the
/// dev-loop bootstrap user message. Tuned for the upstream Cloudflare
/// WAF in front of `aura-router`; see the field docstring on
/// [`PromptsConfig::bootstrap_spec_bytes`] for the full rationale.
pub const BOOTSTRAP_SPEC_DEFAULT_BYTES: usize = 1500;

/// Leading hex chars from a `content_hash` we surface in the
/// repeated-read nudge. Short enough to keep the message readable,
/// long enough to be unique inside one turn (the read tool stamps a
/// 16-hex `u64` digest).
pub const REPEATED_READ_HASH_DISPLAY_CHARS: usize = 8;

/// Per-block character cap applied while rendering the compaction
/// summary auxiliary prompt body. Larger blocks get
/// `aura_compaction::truncate_content`-trimmed.
pub const PROMPT_COMPACTION_MAX_BLOCK_CHARS: usize = 4_000;

// ---------------------------------------------------------------------------
// Config struct
// ---------------------------------------------------------------------------

/// Prompt-layer config.
#[derive(Debug, Clone, Copy)]
pub struct PromptsConfig {
    /// env: `AURA_AGENT_BOOTSTRAP_SPEC_BYTES` (default: `1500`)
    ///
    /// Bytes of `spec.markdown_contents` the dev-loop bootstrap user
    /// message keeps before truncating. `0` skips the spec body.
    pub bootstrap_spec_bytes: usize,
    /// env: `AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES` (default: `false`)
    ///
    /// When `true`, fenced code blocks are stripped from spec markdown
    /// and task descriptions before injection (WAF-safety knob).
    pub bootstrap_strip_code_fences: bool,
    /// env: `AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS` (default: `12_000`)
    ///
    /// Maximum task-context characters retained when capping the
    /// bootstrap context before routing to the LLM.
    pub bootstrap_context_chars: usize,
    /// Compile-time only (default:
    /// [`REPEATED_READ_HASH_DISPLAY_CHARS`] = `8`). Leading hex chars
    /// displayed in the repeated-read nudge.
    pub repeated_read_hash_display_chars: usize,
    /// Compile-time only (default:
    /// [`PROMPT_COMPACTION_MAX_BLOCK_CHARS`] = `4_000`). Per-block
    /// character cap for the compaction summary prompt.
    pub compaction_max_block_chars: usize,
}

impl PromptsConfig {
    /// Compile-time defaults.
    #[must_use]
    pub const fn defaults() -> Self {
        Self {
            bootstrap_spec_bytes: BOOTSTRAP_SPEC_DEFAULT_BYTES,
            bootstrap_strip_code_fences: false,
            bootstrap_context_chars: crate::agent::DEFAULT_BOOTSTRAP_TASK_CONTEXT_CHARS,
            repeated_read_hash_display_chars: REPEATED_READ_HASH_DISPLAY_CHARS,
            compaction_max_block_chars: PROMPT_COMPACTION_MAX_BLOCK_CHARS,
        }
    }

    /// Apply env overrides.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ConfigError`] when one of the numeric
    /// overrides is non-empty but unparseable.
    pub fn from_env() -> Result<Self, crate::ConfigError> {
        let mut cfg = Self::defaults();
        if let Some(spec_bytes) = lookup_numeric::<usize>(AURA_AGENT_BOOTSTRAP_SPEC_BYTES)? {
            cfg.bootstrap_spec_bytes = spec_bytes;
        }
        cfg.bootstrap_strip_code_fences = lookup_bool(
            AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES,
            false,
            TRUTHY_LITERALS,
            &[],
        );
        if let Some(ctx) = lookup_numeric::<usize>(AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS)? {
            cfg.bootstrap_context_chars = ctx;
        }
        Ok(cfg)
    }
}

impl Default for PromptsConfig {
    fn default() -> Self {
        Self::defaults()
    }
}
