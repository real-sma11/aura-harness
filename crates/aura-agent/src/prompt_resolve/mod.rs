//! IO half of the iteration-0 enrichment block.
//!
//! Phase 2 split: the pure regex extraction + markdown rendering live
//! in `aura-prompts/src/enrichment/` (no IO, no async). This module
//! owns the matching IO half:
//!
//! - The [`WorkspaceReader`] trait — async hooks for `exists`,
//!   `read_file_head`, `grep_definition`, and `discover_module_paths`.
//! - The real [`fs_workspace::FsWorkspace`] implementation rooted at
//!   the project's working directory.
//! - The async [`resolve_hints`] orchestrator that consumes pure
//!   `aura_prompts::enrichment::ContextHints` and returns
//!   `aura_prompts::enrichment::ResolvedContext` for rendering.

use async_trait::async_trait;

pub use aura_prompts::enrichment::types::{
    ContextHints, ResolveCaps, ResolvedContext, ResolvedPath, ResolvedSymbol, SymbolHit,
};
pub use aura_prompts::enrichment::{default_caps, extract_hints};

mod fs_workspace;

pub use fs_workspace::FsWorkspace;

/// Workspace IO surface needed by [`resolve_hints`]. Trait-shaped so
/// tests can stub it without touching the real filesystem or shelling
/// out to ripgrep.
#[async_trait]
pub trait WorkspaceReader: Send + Sync {
    /// True iff `relative_path` resolves to an existing file under
    /// the workspace root.
    async fn exists(&self, relative_path: &str) -> bool;

    /// Return up to `max_lines` lines from the head of
    /// `relative_path`, or `None` on any read error / missing file.
    async fn read_file_head(&self, relative_path: &str, max_lines: usize) -> Option<String>;

    /// Find up to `max_hits` definition-shaped matches for `symbol`
    /// across the workspace. A "definition" is any line matching one
    /// of `fn|struct|trait|enum|type|const|impl <symbol>` (with
    /// optional `pub`/`async` prefix). Symbols of the form `Foo::bar`
    /// search for `bar` first and fall back to `Foo`.
    async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit>;

    /// Find up to `max_hits` workspace-relative `**/src/{module}.rs`
    /// paths. Default impl returns empty (test stubs).
    async fn discover_module_paths(&self, module: &str, max_hits: usize) -> Vec<String> {
        let _ = (module, max_hits);
        Vec::new()
    }
}

/// Resolve `hints` against `workspace`, honouring `caps`. Best-effort:
/// missing files / failed reads / grep timeouts silently drop that
/// hint. Construction of the agent context must never block on the
/// workspace, so every IO call is wrapped in
/// `caps.per_call_timeout`.
pub async fn resolve_hints<R: WorkspaceReader + ?Sized>(
    hints: &ContextHints,
    workspace: &R,
    caps: ResolveCaps,
) -> ResolvedContext {
    let mut path_candidates = hints.paths.clone();
    for kw in &hints.module_keywords {
        let discovered = tokio::time::timeout(
            caps.per_call_timeout,
            workspace.discover_module_paths(kw, 2),
        )
        .await
        .unwrap_or_default();
        for path in discovered {
            if !path_candidates.contains(&path) {
                path_candidates.push(path);
            }
        }
    }

    let mut resolved_paths = Vec::new();
    for path in path_candidates.iter().take(caps.max_paths) {
        let exists = tokio::time::timeout(caps.per_call_timeout, workspace.exists(path))
            .await
            .unwrap_or(false);
        if !exists {
            continue;
        }
        let head = tokio::time::timeout(
            caps.per_call_timeout,
            workspace.read_file_head(path, caps.max_lines_per_path),
        )
        .await
        .ok()
        .flatten();
        let head_line_count = head
            .as_deref()
            .map(|s| s.lines().count())
            .unwrap_or(caps.max_lines_per_path);
        resolved_paths.push(ResolvedPath {
            path: path.clone(),
            head,
            head_line_count,
        });
    }

    let mut resolved_symbols = Vec::new();
    for symbol in hints.symbols.iter().take(caps.max_symbols) {
        let hits =
            tokio::time::timeout(caps.per_call_timeout, workspace.grep_definition(symbol, 3))
                .await
                .ok()
                .unwrap_or_default();
        if !hits.is_empty() {
            resolved_symbols.push(ResolvedSymbol {
                symbol: symbol.clone(),
                hits,
            });
        }
    }

    ResolvedContext {
        paths: resolved_paths,
        symbols: resolved_symbols,
        module_note: hints.module_note.clone(),
        max_block_chars: caps.max_block_chars,
    }
}

#[cfg(test)]
mod tests;
