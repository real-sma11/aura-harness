//! Pure regex-based extraction of [`super::ContextHints`] from a task
//! description.
//!
//! Each submodule owns one extraction shape (paths, symbols, modules)
//! and caches its compiled `Regex` instances with
//! [`std::sync::OnceLock`] (Phase 0 debt collection per §2.3 of the
//! refactor plan). The crate-level [`extract_hints`] entry point
//! composes the three extractors plus the dedup / module-expansion
//! pass and returns a populated [`super::ContextHints`].

use std::collections::HashSet;

use super::types::ContextHints;

pub mod modules;
pub mod paths;
pub mod symbols;

/// Extract candidate paths and symbol names from a task description.
///
/// Heuristics (intentionally cheap):
/// - Paths: `\b(crates|apps|src|tests|examples)/<path>\.<ext>\b` plus
///   any backtick- or quote-wrapped path-shaped token.
/// - Symbols: four cheap regex passes deduped through a `HashSet`:
///   CamelCase-prefixed Rust paths (`Foo::bar`), snake_case-prefixed
///   Rust paths (`zero_storage::Outbox`), backtick-wrapped
///   identifiers (`` `enqueue_batch` ``), and bare CamelCase
///   identifiers in prose (`Publisher`, `OutboxEntry`, `URL`).
/// - Filters HTTP(s) URLs and common English words.
///
/// Order of first appearance is preserved; duplicates are dropped.
#[must_use]
pub fn extract_hints(description: &str) -> ContextHints {
    let mut hints = extract_hints_core(description);
    modules::expand_module_hints(description, &mut hints);
    hints
}

fn extract_hints_core(description: &str) -> ContextHints {
    let mut paths = ordered_unique(paths::extract_paths(description));
    let mut symbols = ordered_unique(symbols::extract_symbols(description));
    let path_set: HashSet<&str> = paths.iter().map(String::as_str).collect();
    symbols.retain(|s| !path_set.contains(s.as_str()));
    paths.truncate(64);
    symbols.truncate(64);
    ContextHints {
        paths,
        symbols,
        module_keywords: Vec::new(),
        module_note: None,
    }
}

pub(super) fn ordered_unique<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}
