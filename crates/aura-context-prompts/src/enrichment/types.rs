//! Plain data types exchanged between the pure-prompt half and the
//! IO half (`aura-agent/src/prompt_resolve/`).
//!
//! Every type here is free of trait objects, async, and IO-handles.
//! `aura-prompts` produces a [`ContextHints`] from the task
//! description, hands it to the agent-side resolver, then renders
//! whatever [`ResolvedContext`] comes back.

use std::time::Duration;

/// Candidate hints scraped out of a task description.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContextHints {
    /// Workspace-relative path candidates (e.g. `crates/foo/src/lib.rs`).
    pub paths: Vec<String>,
    /// Code-symbol candidates (e.g. `Outbox::enqueue`, `RetryPolicy`).
    pub symbols: Vec<String>,
    /// Lowercase module/file stem keywords (`outbox`, `inbox`, …).
    pub module_keywords: Vec<String>,
    /// Optional note spliced into the enrichment block (missing-file hints).
    pub module_note: Option<String>,
}

impl ContextHints {
    /// True iff we have at least one candidate worth resolving.
    #[must_use]
    pub fn is_meaningful(&self) -> bool {
        !self.paths.is_empty() || !self.symbols.is_empty() || !self.module_keywords.is_empty()
    }
}

/// Caps applied to a single `resolve_hints` pass.
#[derive(Debug, Clone, Copy)]
pub struct ResolveCaps {
    /// Maximum number of candidate paths to attempt to resolve.
    pub max_paths: usize,
    /// Maximum number of candidate symbols to attempt to resolve.
    pub max_symbols: usize,
    /// Maximum lines of file head to capture per resolved path.
    pub max_lines_per_path: usize,
    /// Soft cap on the total emitted markdown block size, in chars.
    /// When exceeded, file-head bodies are dropped (lowest-priority
    /// first) while the path/symbol lists are preserved.
    pub max_block_chars: usize,
    /// Per-call IO timeout. A slow exists / read / grep call falls
    /// back to dropping that hint rather than blocking context
    /// construction.
    pub per_call_timeout: Duration,
}

impl Default for ResolveCaps {
    fn default() -> Self {
        Self {
            max_paths: 8,
            max_symbols: 6,
            max_lines_per_path: 40,
            max_block_chars: 4000,
            per_call_timeout: Duration::from_secs(2),
        }
    }
}

/// A single matched symbol definition site (path + line + line text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    /// Workspace-relative path of the source file.
    pub path: String,
    /// 1-based source line number of the definition.
    pub line: usize,
    /// Trimmed line text the regex matched against.
    pub text: String,
}

/// Output of `resolve_hints`: a structured snapshot of what was
/// resolved against the workspace. Render with
/// [`crate::enrichment::render::render_block`] (or
/// [`ResolvedContext::into_block`]).
#[derive(Debug, Default, Clone)]
pub struct ResolvedContext {
    /// Resolved file paths and their optional file-head previews.
    pub paths: Vec<ResolvedPath>,
    /// Resolved symbols (one per input symbol that matched at least
    /// one definition).
    pub symbols: Vec<ResolvedSymbol>,
    /// Optional module-level note threaded through from
    /// [`ContextHints::module_note`].
    pub module_note: Option<String>,
    /// Soft budget enforced by [`Self::into_block`]. Carried on the
    /// snapshot so the renderer doesn't need a separate caps
    /// argument.
    pub max_block_chars: usize,
}

impl ResolvedContext {
    /// True iff there's nothing to render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.symbols.is_empty() && self.module_note.is_none()
    }

    /// Render the resolved hints as a markdown block ready to
    /// splice into the agent's initial user message. Returns the
    /// empty string when [`Self::is_empty`] is true so callers can
    /// splice unconditionally.
    ///
    /// Honours `max_block_chars`: when the full rendering exceeds
    /// the budget, the lowest-priority file-head bodies are dropped
    /// (the path list and symbol list always stay) until the block
    /// fits.
    #[must_use]
    pub fn into_block(self) -> String {
        super::render::into_block(&self)
    }
}

/// One resolved path entry inside [`ResolvedContext`].
#[derive(Debug, Clone)]
pub struct ResolvedPath {
    /// Workspace-relative path that resolved to a real file.
    pub path: String,
    /// Optional file-head preview (`max_lines_per_path` lines from
    /// the top of the file). `None` when the head read failed or
    /// timed out.
    pub head: Option<String>,
    /// Number of lines actually captured in [`Self::head`]. Falls
    /// back to `caps.max_lines_per_path` when head is absent so the
    /// rendered "lines 1-N" tag still names a sensible value.
    pub head_line_count: usize,
}

/// One resolved symbol entry inside [`ResolvedContext`].
#[derive(Debug, Clone)]
pub struct ResolvedSymbol {
    /// The original symbol candidate from
    /// [`ContextHints::symbols`].
    pub symbol: String,
    /// Definition-shaped grep hits the resolver found.
    pub hits: Vec<SymbolHit>,
}
