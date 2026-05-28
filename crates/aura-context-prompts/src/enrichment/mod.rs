//! Pre-resolve files & symbols mentioned in a task description and
//! render them as a markdown block to splice into the first-attempt
//! task context.
//!
//! Phase 2 split: the **pure** half lives here (regex extraction
//! over the task description, plus the markdown renderer for an
//! already-resolved [`ResolvedContext`]). The **IO** half — the
//! `WorkspaceReader` trait, the real `FsWorkspace`, and the async
//! `resolve_hints` orchestration — lives in
//! `aura-agent/src/prompt_resolve/`. The two halves communicate
//! through the plain data types in [`types`].
//!
//! By the time the model lands on iteration 0 it already knows which
//! files exist, where they live, and what their relevant signatures
//! look like — so the explore phase is a short verification pass
//! instead of a multi-iteration grep crawl.

pub mod extract;
pub mod render;
pub mod types;

pub use extract::extract_hints;
pub use render::render_block;
pub use types::{
    ContextHints, ResolveCaps, ResolvedContext, ResolvedPath, ResolvedSymbol, SymbolHit,
};

/// Compile-time defaults for [`ResolveCaps`] used by the agent
/// runner. Pulled out so callers don't need to reach into
/// [`types::ResolveCaps::default`].
#[must_use]
pub fn default_caps() -> ResolveCaps {
    ResolveCaps::default()
}

#[cfg(test)]
mod tests;
