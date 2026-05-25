//! Pre-resolve files & symbols mentioned in a task description and
//! splice the results into the first-attempt task context.
//!
//! Mirrors Codex's `apply_patch` seeding pattern: by the time the model
//! lands on iteration 0 it already knows which files exist, where they
//! live, and what their relevant signatures look like — so the explore
//! phase is a short verification pass instead of a multi-iteration grep
//! crawl.
//!
//! ## Scope
//! - Heuristic, not LLM-driven: a couple of cheap regex passes over the
//!   task description.
//! - Best-effort: any IO failure (missing file, slow grep, timeout)
//!   silently drops that hint. Construction of the agent context must
//!   never block on the workspace.
//! - Per-task, never cached: different tasks have different hints.
//!
//! ## Caps
//! Defaults: 8 paths, 6 symbols, 40 lines per path, 4000-char total
//! block, 2-second per-call timeout. Tune via [`ResolveCaps`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use walkdir::WalkDir;

/// Candidate hints scraped out of a task description.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContextHints {
    /// Workspace-relative path candidates (e.g. `crates/foo/src/lib.rs`).
    pub paths: Vec<String>,
    /// Code-symbol candidates (e.g. `Outbox::enqueue`, `RetryPolicy`).
    pub symbols: Vec<String>,
}

impl ContextHints {
    /// True iff we have at least one candidate worth resolving.
    #[must_use]
    pub fn is_meaningful(&self) -> bool {
        !self.paths.is_empty() || !self.symbols.is_empty()
    }
}

/// Caps applied to a single [`resolve_hints`] pass.
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
    /// Per-call IO timeout. A slow exists / read / grep call falls back
    /// to dropping that hint rather than blocking context construction.
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

/// Return the default [`ResolveCaps`] used by the agent runner.
#[must_use]
pub fn default_caps() -> ResolveCaps {
    ResolveCaps::default()
}

/// A single matched symbol definition site (path + line + line text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

/// Workspace IO surface needed by [`resolve_hints`]. Trait-shaped so
/// tests can stub it without touching the real filesystem or shelling
/// out to ripgrep.
#[async_trait]
pub trait WorkspaceReader: Send + Sync {
    /// True iff `relative_path` resolves to an existing file under the
    /// workspace root.
    async fn exists(&self, relative_path: &str) -> bool;

    /// Return up to `max_lines` lines from the head of `relative_path`,
    /// or `None` on any read error / missing file.
    async fn read_file_head(&self, relative_path: &str, max_lines: usize) -> Option<String>;

    /// Find up to `max_hits` definition-shaped matches for `symbol`
    /// across the workspace. A "definition" is any line matching one of
    /// `fn|struct|trait|enum|type|const|impl <symbol>` (with optional
    /// `pub`/`async` prefix). Symbols of the form `Foo::bar` search for
    /// `bar` first and fall back to `Foo`.
    async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit>;
}

/// Real-filesystem implementation of [`WorkspaceReader`], rooted at a
/// project folder path. Uses `tokio::fs` for reads and a synchronous
/// `walkdir + regex` pass on a blocking pool for definition grep so
/// the tokio reactor never blocks on a large workspace walk.
pub struct FsWorkspace {
    root: PathBuf,
}

impl FsWorkspace {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// Directories we never recurse into during definition grep. Matches
/// the `search_code` tool's skip list — these are universally noise
/// (vendored deps, build artefacts, VCS metadata) and walking them
/// makes the grep blow past its timeout on real projects.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    "dist",
    "build",
    ".next",
    "vendor",
    ".venv",
    "coverage",
    ".tox",
    ".mypy_cache",
];

#[async_trait]
impl WorkspaceReader for FsWorkspace {
    async fn exists(&self, relative_path: &str) -> bool {
        let full = self.root.join(relative_path);
        tokio::fs::metadata(&full)
            .await
            .is_ok_and(|m| m.is_file())
    }

    async fn read_file_head(&self, relative_path: &str, max_lines: usize) -> Option<String> {
        let full = self.root.join(relative_path);
        let content = tokio::fs::read_to_string(&full).await.ok()?;
        Some(
            content
                .lines()
                .take(max_lines)
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
        let root = self.root.clone();
        let symbol = symbol.to_string();
        tokio::task::spawn_blocking(move || grep_definition_blocking(&root, &symbol, max_hits))
            .await
            .unwrap_or_default()
    }
}

/// Synchronous workspace walk for [`FsWorkspace::grep_definition`].
/// Restricted to `.rs` files because the symbol regex
/// (`fn|struct|trait|enum|...`) is Rust-syntax-specific. Adding
/// language detection here is out of scope for the seeding hint —
/// false negatives on non-Rust files just mean the model has to grep
/// itself, which is the no-op baseline.
fn grep_definition_blocking(root: &Path, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
    if max_hits == 0 {
        return Vec::new();
    }
    // `Foo::bar` -> search for `bar` (the method/assoc-fn name) first.
    // If that yields nothing, fall back to `Foo` (the type/trait name).
    // Bare symbols (no `::`) just search for themselves.
    let (primary, fallback) = match symbol.split_once("::") {
        Some((head, tail)) if !tail.is_empty() => (tail.to_string(), Some(head.to_string())),
        _ => (symbol.to_string(), None),
    };
    let mut hits = grep_one(root, &primary, max_hits);
    if hits.is_empty() {
        if let Some(fallback) = fallback {
            hits = grep_one(root, &fallback, max_hits);
        }
    }
    hits
}

fn grep_one(root: &Path, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
    let pattern = format!(
        r"\b(?:pub(?:\([^)]*\))?\s+)?(?:async\s+|unsafe\s+|const\s+)*(?:fn|struct|trait|enum|type|impl|const|static|mod)\s+{}\b",
        regex::escape(symbol)
    );
    let re = match Regex::new(&pattern) {
        Ok(re) => re,
        Err(_) => return Vec::new(),
    };
    let mut hits = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                !SKIP_DIRS.contains(&name.as_ref())
            } else {
                true
            }
        })
        .filter_map(Result::ok)
    {
        if hits.len() >= max_hits {
            break;
        }
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(p) else {
            continue;
        };
        for (idx, line) in content.lines().enumerate() {
            if re.is_match(line) {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/");
                hits.push(SymbolHit {
                    path: rel,
                    line: idx + 1,
                    text: line.trim().to_string(),
                });
                if hits.len() >= max_hits {
                    break;
                }
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------------
// Hint extraction
// ---------------------------------------------------------------------------

/// English stopwords and Rust-keyword-shaped tokens that the
/// snake-case backtick regex picks up but that have ~zero chance of
/// being a real workspace symbol. Keep this list small and high-signal;
/// the cost of a few false-positive grep calls is much lower than the
/// cost of dropping a real symbol.
///
/// The trailing block is a deliberate, minimal expansion to cover
/// sentence-initial English verbs/nouns that the new `bare_camel_regex`
/// picks up but that have ~zero chance of being a real workspace
/// symbol. Without these, prose like "Refactor the engine" or "The
/// Implementation Then Defines An Outbox" leaks `Refactor`,
/// `Implementation`, `Then`, `Defines` into the symbol list. Kept
/// minimal: only words that previously caused a test regression are
/// listed here. Future additions should be justified in the same way
/// (a real input that the harness saw + a regression test pinning the
/// fix).
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "this", "that", "from", "into", "into_",
    "self", "true", "false", "none", "some", "ok", "err", "fn", "pub",
    "impl", "struct", "trait", "enum", "type", "mod", "use", "let", "mut",
    "ref", "where", "match", "if", "else", "loop", "while", "for_each",
    "return", "break", "continue", "as", "in", "of", "on", "to", "at",
    "by", "or", "not", "is", "be", "do", "we", "you", "it", "an", "a",
    "todo", "fixme", "note", "tip", "warning", "see", "test", "tests",
    // bare_camel_regex (Phase A) coverage:
    "refactor", "implement", "implementation", "then", "defines", "define",
];

/// Extract candidate paths and symbol names from a task description.
///
/// Heuristics (intentionally cheap):
/// - Paths: `\b(crates|apps|src|tests|examples)/<path>\.<ext>\b` plus
///   any backtick- or quote-wrapped path-shaped token.
/// - Symbols: four cheap regex passes deduped through a `HashSet`:
///   CamelCase-prefixed Rust paths (`Foo::bar`), snake_case-prefixed
///   Rust paths (`zero_storage::Outbox` — Cargo crates are
///   conventionally snake_case), backtick-wrapped identifiers
///   (`` `enqueue_batch` ``), and bare CamelCase identifiers in
///   prose (`Publisher`, `OutboxEntry`, `URL`).
/// - Filters HTTP(s) URLs, common English words, and tokens whose
///   "extension" is actually a sentence terminator.
///
/// Order of first appearance is preserved; duplicates are dropped.
#[must_use]
pub fn extract_hints(description: &str) -> ContextHints {
    let mut paths = ordered_unique(extract_paths(description));
    let mut symbols = ordered_unique(extract_symbols(description));
    // Don't double-count a path-shaped token as a symbol when it
    // already landed in `paths`.
    let path_set: HashSet<&str> = paths.iter().map(String::as_str).collect();
    symbols.retain(|s| !path_set.contains(s.as_str()));
    // Defensively cap before resolve: extract is cheap but a giant
    // description with hundreds of paths would still drag.
    paths.truncate(64);
    symbols.truncate(64);
    ContextHints { paths, symbols }
}

fn ordered_unique<I: IntoIterator<Item = String>>(items: I) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// Match `crates/foo/src/bar.rs`, `apps/x/y.ts`, `src/lib.rs`, etc.
/// Extensions are restricted to 1-6 word-chars to avoid matching
/// sentences (e.g. "src/main.rs." would have stopped at `rs`).
fn path_top_level_regex() -> Regex {
    Regex::new(r"(?x)
        \b
        (?:crates|apps|src|tests|examples|docs|scripts|benches|bin)
        /
        [\w./-]+?
        \.
        [A-Za-z][A-Za-z0-9]{0,5}
        \b
    ")
    .expect("path_top_level_regex must compile")
}

/// Match backtick- or double-quote-wrapped tokens whose body looks
/// like a path (contains a `/` and ends in `.ext`).
fn quoted_path_regex() -> Regex {
    Regex::new(r#"(?x)
        [`"]
        ([A-Za-z0-9_./-]+ / [A-Za-z0-9_./-]* \. [A-Za-z][A-Za-z0-9]{0,5})
        [`"]
    "#)
    .expect("quoted_path_regex must compile")
}

fn extract_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let top = path_top_level_regex();
    let quoted = quoted_path_regex();
    for m in top.find_iter(text) {
        let s = m.as_str();
        if !is_url_like(s) && !looks_like_sentence_punctuation(s) {
            out.push(s.to_string());
        }
    }
    for cap in quoted.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            if !is_url_like(s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn is_url_like(s: &str) -> bool {
    s.contains("://") || s.starts_with("http") && s.contains('/')
}

/// Drop tokens like `Sec.tion` (where the "extension" is just one word
/// char and the prefix has no slash). The top-level regex already
/// requires a leading directory prefix, so this is a defense-in-depth
/// guard for path-shaped sentence fragments.
fn looks_like_sentence_punctuation(_s: &str) -> bool {
    false
}

/// Match Rust `Module::item`-shaped paths whose leading segment is
/// CamelCase — i.e. a type/trait/module conventionally named that way.
fn rust_path_camel_regex() -> Regex {
    Regex::new(r"\b[A-Z][A-Za-z0-9_]+::[A-Za-z0-9_]+\b")
        .expect("rust_path_camel_regex must compile")
}

/// Match Rust `crate::item`-shaped paths whose leading segment is
/// snake_case. Cargo crate names are conventionally snake_case
/// (`zero_storage`, `tokio`, `serde_json`), so any Rust path that
/// *starts* at a crate boundary necessarily has a lowercase leading
/// segment. The original `rust_path_regex` only matched CamelCase
/// prefixes and silently dropped `zero_storage::Outbox`-style symbols
/// referenced in real dev-loop task descriptions (the Publisher task
/// that motivated this change). Greedy `(?:::ident)+` so chained
/// paths like `zero_network::publisher::PublisherHandle` come back as
/// a single match rather than truncated to the first two segments.
fn rust_path_snake_regex() -> Regex {
    Regex::new(r"\b[a-z][a-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+\b")
        .expect("rust_path_snake_regex must compile")
}

fn backtick_ident_regex() -> Regex {
    // CamelCase or snake_case identifier inside backticks. We
    // intentionally exclude function-call shapes (`foo(`), paths
    // (`foo/bar`), and macro shapes (`foo!`) — those carry too many
    // false positives.
    Regex::new(r"`([A-Za-z_][A-Za-z0-9_]*)`")
        .expect("backtick_ident_regex must compile")
}

/// Match bare (unquoted, no `::`) CamelCase identifiers in prose,
/// e.g. `Publisher`, `OutboxEntry`, `PublisherHandle`, `URL`. Three
/// shapes, alternation order chosen so leftmost-first matching prefers
/// the longest CamelCase span:
/// 1. CompoundCamelCase (≥2 humps): `OutboxEntry`, `MockGridClient`,
///    `PublisherHandle`, `RetryPolicy`. Highest-signal — multiple
///    case transitions are essentially never an English word.
/// 2. Single-hump CamelCase (Cap + ≥2 lowercase): `Publisher`,
///    `Outbox`. The riskiest shape — sentence-initial English words
///    (`Refactor`, `Implementation`, `Defines`) land here too, so
///    [`STOPWORDS`] is expanded to filter the most common offenders
///    and [`is_plausible_camel_ident`] enforces it.
/// 3. Short all-caps acronyms (length 3-6): `URL`, `HTTP`, `JSON`.
fn bare_camel_regex() -> Regex {
    Regex::new(r"\b(?:[A-Z][a-z]+(?:[A-Z][a-z]*|[A-Z]+)+|[A-Z][a-z]{2,}|[A-Z]{3,6})\b")
        .expect("bare_camel_regex must compile")
}

fn extract_symbols(text: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for m in rust_path_camel_regex().find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for m in rust_path_snake_regex().find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for cap in backtick_ident_regex().captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            if is_plausible_ident(s) {
                let owned = s.to_string();
                if seen.insert(owned.clone()) {
                    out.push(owned);
                }
            }
        }
    }
    for m in bare_camel_regex().find_iter(text) {
        let s = m.as_str();
        if is_plausible_camel_ident(s) {
            let owned = s.to_string();
            if seen.insert(owned.clone()) {
                out.push(owned);
            }
        }
    }

    out
}

fn is_plausible_ident(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if STOPWORDS.contains(&lower.as_str()) {
        return false;
    }
    // Need at least one uppercase letter OR an underscore — bare
    // lowercase words like `enqueue` would also match, but they're
    // common English-ish nouns and produce mostly noise without context.
    s.chars().any(|c| c.is_ascii_uppercase()) || s.contains('_')
}

/// Filter for [`bare_camel_regex`] matches. The regex itself already
/// guarantees shape (≥3 chars, leading uppercase); this enforces the
/// STOPWORDS denylist that suppresses sentence-initial English words
/// the regex can't structurally tell apart from real symbols.
fn is_plausible_camel_ident(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    !STOPWORDS.contains(&lower.as_str())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Output of [`resolve_hints`]: a structured snapshot of what we found.
/// Render with [`ResolvedContext::into_block`].
#[derive(Debug, Default, Clone)]
pub struct ResolvedContext {
    paths: Vec<ResolvedPath>,
    symbols: Vec<ResolvedSymbol>,
    /// Soft budget enforced by [`Self::into_block`].
    max_block_chars: usize,
}

#[derive(Debug, Clone)]
struct ResolvedPath {
    path: String,
    head: Option<String>,
    head_line_count: usize,
}

#[derive(Debug, Clone)]
struct ResolvedSymbol {
    symbol: String,
    hits: Vec<SymbolHit>,
}

impl ResolvedContext {
    /// True iff there's nothing to render.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.symbols.is_empty()
    }

    /// Render the resolved hints as a markdown block ready to splice
    /// into the agent's initial user message. Returns the empty string
    /// when [`Self::is_empty`] is true so callers can splice
    /// unconditionally.
    ///
    /// Honours `max_block_chars`: when the full rendering exceeds the
    /// budget, the lowest-priority file-head bodies are dropped (the
    /// path list and symbol list always stay) until the block fits.
    #[must_use]
    pub fn into_block(self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let max_chars = self.max_block_chars;
        let mut path_bodies_kept: Vec<bool> = vec![true; self.paths.len()];
        loop {
            let rendered = render_block(&self.paths, &self.symbols, &path_bodies_kept);
            if rendered.len() <= max_chars || max_chars == 0 {
                return rendered;
            }
            // Drop the lowest-priority body that's still being rendered.
            let Some(drop_idx) = path_bodies_kept
                .iter()
                .enumerate()
                .rev()
                .find(|(_, kept)| **kept)
                .map(|(i, _)| i)
            else {
                // Everything's already body-less; give up enforcing
                // the budget rather than truncate mid-token.
                return rendered;
            };
            path_bodies_kept[drop_idx] = false;
        }
    }
}

fn render_block(
    paths: &[ResolvedPath],
    symbols: &[ResolvedSymbol],
    path_bodies_kept: &[bool],
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    out.push_str("## Pre-resolved context (from task description)\n\n");

    if !paths.is_empty() {
        out.push_str("Files mentioned in the task that exist in the workspace:\n");
        for (i, p) in paths.iter().enumerate() {
            let body_kept = *path_bodies_kept.get(i).unwrap_or(&false);
            if body_kept && p.head.is_some() {
                let _ = writeln!(
                    out,
                    "- `{}` (file head, lines 1-{} below)",
                    p.path, p.head_line_count
                );
            } else {
                let _ = writeln!(out, "- `{}`", p.path);
            }
        }
        out.push('\n');

        for (i, p) in paths.iter().enumerate() {
            if !*path_bodies_kept.get(i).unwrap_or(&false) {
                continue;
            }
            let Some(head) = &p.head else {
                continue;
            };
            let _ = writeln!(out, "### {} (lines 1-{})", p.path, p.head_line_count);
            out.push_str("```rust\n");
            out.push_str(head);
            if !head.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("```\n\n");
        }
    }

    if !symbols.is_empty() {
        out.push_str("Symbols referenced in the task:\n");
        for s in symbols {
            if let Some(first) = s.hits.first() {
                let _ = writeln!(out, "- `{}` -> {}:{}", s.symbol, first.path, first.line);
                for extra in s.hits.iter().skip(1) {
                    let _ = writeln!(out, "  also: {}:{}", extra.path, extra.line);
                }
            }
        }
        out.push('\n');
    }

    out.push_str(
        "Use these as starting points; you do NOT need to re-list the \
         directory or re-grep for these symbols.\n",
    );
    out
}

/// Resolve `hints` against `workspace`, honouring `caps`. Best-effort:
/// missing files / failed reads / grep timeouts silently drop that
/// hint. Construction of the agent context must never block on the
/// workspace, so every IO call is wrapped in
/// [`ResolveCaps::per_call_timeout`].
pub async fn resolve_hints<R: WorkspaceReader + ?Sized>(
    hints: &ContextHints,
    workspace: &R,
    caps: ResolveCaps,
) -> ResolvedContext {
    let mut resolved_paths = Vec::new();
    for path in hints.paths.iter().take(caps.max_paths) {
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
        let hits = tokio::time::timeout(
            caps.per_call_timeout,
            workspace.grep_definition(symbol, 3),
        )
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
        max_block_chars: caps.max_block_chars,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory [`WorkspaceReader`] stub. Tests configure `files`
    /// (path -> contents) and `definitions` (symbol -> hits) and the
    /// stub answers from those maps. No real filesystem access.
    #[derive(Default)]
    struct StubWorkspace {
        files: Mutex<HashMap<String, String>>,
        definitions: Mutex<HashMap<String, Vec<SymbolHit>>>,
    }

    impl StubWorkspace {
        fn with_file(self, path: &str, body: &str) -> Self {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), body.to_string());
            self
        }

        fn with_definition(self, symbol: &str, hits: Vec<SymbolHit>) -> Self {
            self.definitions
                .lock()
                .unwrap()
                .insert(symbol.to_string(), hits);
            self
        }
    }

    #[async_trait]
    impl WorkspaceReader for StubWorkspace {
        async fn exists(&self, relative_path: &str) -> bool {
            self.files.lock().unwrap().contains_key(relative_path)
        }

        async fn read_file_head(
            &self,
            relative_path: &str,
            max_lines: usize,
        ) -> Option<String> {
            self.files.lock().unwrap().get(relative_path).map(|body| {
                body.lines()
                    .take(max_lines)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }

        async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
            self.definitions
                .lock()
                .unwrap()
                .get(symbol)
                .cloned()
                .map(|v| v.into_iter().take(max_hits).collect())
                .unwrap_or_default()
        }
    }

    #[test]
    fn extract_hints_picks_paths_and_symbols() {
        let desc = "Wire Publisher::enqueue in crates/zero-network/src/publisher.rs \
                    to spawn_driver in crates/zero-storage/src/outbox.rs. The \
                    `Outbox` type already exists; reuse its `enqueue_batch` helper.";
        let hints = extract_hints(desc);
        assert!(
            hints.paths.contains(&"crates/zero-network/src/publisher.rs".to_string()),
            "expected publisher.rs in paths, got {:?}",
            hints.paths
        );
        assert!(
            hints.paths.contains(&"crates/zero-storage/src/outbox.rs".to_string()),
            "expected outbox.rs in paths, got {:?}",
            hints.paths
        );
        assert!(
            hints.symbols.contains(&"Publisher::enqueue".to_string()),
            "expected Publisher::enqueue in symbols, got {:?}",
            hints.symbols
        );
        assert!(
            hints.symbols.contains(&"Outbox".to_string()),
            "expected Outbox in symbols, got {:?}",
            hints.symbols
        );
        assert!(
            hints.symbols.contains(&"enqueue_batch".to_string()),
            "expected enqueue_batch (backtick snake_case) in symbols, got {:?}",
            hints.symbols
        );
        assert!(hints.is_meaningful());
    }

    #[test]
    fn extract_hints_rejects_http_urls_and_english_words() {
        let desc = "See https://example.com/foo/bar.txt and the docs at \
                    http://docs.rs/regex/latest/regex/struct.Regex.html. The \
                    `and` and `for` keywords are not symbols; neither is `fn`.";
        let hints = extract_hints(desc);
        for p in &hints.paths {
            assert!(
                !p.contains("example.com"),
                "URL path leaked into hints: {p}"
            );
            assert!(
                !p.starts_with("http"),
                "URL leaked into hints: {p}"
            );
        }
        for s in &hints.symbols {
            assert_ne!(s, "and");
            assert_ne!(s, "for");
            assert_ne!(s, "fn");
        }
    }

    #[test]
    fn extract_hints_is_empty_for_plain_prose() {
        let desc = "Refactor the engine to be faster and cleaner.";
        let hints = extract_hints(desc);
        assert!(!hints.is_meaningful(), "got {hints:?}");
    }

    #[test]
    fn extracts_snake_case_crate_path() {
        // Two real-world shapes: `crate::Type` and chained
        // `crate::mod::Type`. The original `rust_path_regex` only
        // matched CamelCase prefixes and silently dropped both.
        let desc = "Wire up zero_storage::Outbox and \
                    zero_network::publisher::PublisherHandle to publish.";
        let symbols = extract_symbols(desc);
        assert!(
            symbols.iter().any(|s| s == "zero_storage::Outbox"),
            "expected zero_storage::Outbox in {symbols:?}",
        );
        assert!(
            symbols
                .iter()
                .any(|s| s == "zero_network::publisher::PublisherHandle"),
            "expected zero_network::publisher::PublisherHandle in {symbols:?}",
        );
    }

    #[test]
    fn extracts_bare_camelcase_publisher_task() {
        // Verbatim from the Publisher dev-loop task that motivated
        // Phase A. Before this change, none of the bare CamelCase
        // identifiers (`Publisher`, `Outbox`, `PublisherHandle`,
        // `RetryPolicy`, `MockGridClient`, `FlakyClient`) were
        // extracted unless they appeared in backticks, so iteration 0
        // enrichment had nothing to pre-resolve.
        let desc = "Implement Publisher::enqueue(env) (writes to \
            zero_storage::Outbox then attempts first publish) and \
            spawn_driver() returning a PublisherHandle. Driver loop: \
            poll Outbox::due(now_ms) on a tokio interval, call \
            client.publish, on success mark_sent, on failure \
            record_failure(next_try_ms) per RetryPolicy::next_delay. \
            After 5 failed attempts set next_try_ms = u64::MAX. \
            Acceptance: integration test using MockGridClient wrapped \
            by a FlakyClient that fails the first 3 publishes — \
            driver eventually delivers within \u{2264} attempt 5.";
        let symbols = extract_symbols(desc);
        for expected in [
            "Publisher",
            "Outbox",
            "RetryPolicy",
            "MockGridClient",
            "FlakyClient",
            "PublisherHandle",
        ] {
            assert!(
                symbols.iter().any(|s| s == expected),
                "expected {expected} in {symbols:?}",
            );
        }
    }

    #[test]
    fn does_not_extract_english_words_or_stopwords() {
        // The bare-CamelCase regex would naively match every
        // sentence-initial CamelCase word here. STOPWORDS + the
        // `[A-Z][a-z]{2,}` length floor must filter all five English
        // words while still letting the genuine identifier `Outbox`
        // through.
        let desc = "The Implementation Then Defines An Outbox";
        let symbols = extract_symbols(desc);
        assert!(
            symbols.iter().any(|s| s == "Outbox"),
            "expected Outbox in {symbols:?}",
        );
        for unwanted in ["The", "Implementation", "Then", "Defines", "An"] {
            assert!(
                !symbols.iter().any(|s| s == unwanted),
                "unwanted {unwanted} present in {symbols:?}",
            );
        }
    }

    #[test]
    fn dedupes_symbols_across_regex_sources() {
        // `Outbox` is reachable from three of the four regex sources
        // (backtick, bare CamelCase, and indirectly via the
        // `zero_storage::Outbox` snake-case path). After dedup it
        // should appear exactly once as a standalone symbol.
        let desc = "We use `Outbox` for queueing. The Outbox type \
                    backs zero_storage::Outbox in the storage crate.";
        let symbols = extract_symbols(desc);
        let outbox_count = symbols
            .iter()
            .filter(|s| s.as_str() == "Outbox")
            .count();
        assert_eq!(
            outbox_count, 1,
            "expected exactly one bare Outbox, got {symbols:?}",
        );
        // The `::`-qualified form is a distinct string and should
        // also be present (sanity check that dedup doesn't collapse
        // genuinely different symbols).
        assert!(
            symbols.iter().any(|s| s == "zero_storage::Outbox"),
            "expected zero_storage::Outbox alongside bare Outbox in {symbols:?}",
        );
    }

    #[tokio::test]
    async fn resolve_hints_with_stub_workspace_emits_block() {
        let workspace = StubWorkspace::default()
            .with_file(
                "crates/zero-storage/src/outbox.rs",
                "use crate::prelude::*;\n\npub struct Outbox {\n    inner: Inner,\n}\n",
            )
            .with_definition(
                "Outbox::enqueue",
                vec![SymbolHit {
                    path: "crates/zero-storage/src/outbox.rs".into(),
                    line: 84,
                    text: "pub fn enqueue(&mut self, item: Item) {".into(),
                }],
            );
        let hints = ContextHints {
            paths: vec!["crates/zero-storage/src/outbox.rs".into()],
            symbols: vec!["Outbox::enqueue".into()],
        };
        let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
        assert!(!resolved.is_empty());
        let block = resolved.into_block();
        assert!(block.contains("## Pre-resolved context"));
        assert!(block.contains("crates/zero-storage/src/outbox.rs"));
        assert!(block.contains("pub struct Outbox"));
        assert!(block.contains("Outbox::enqueue"));
        assert!(block.contains("outbox.rs:84"));
        assert!(block.contains("starting points"));
    }

    #[tokio::test]
    async fn resolve_hints_skips_missing_files_silently() {
        let workspace = StubWorkspace::default().with_file(
            "crates/zero-storage/src/outbox.rs",
            "pub struct Outbox;",
        );
        let hints = ContextHints {
            paths: vec![
                "crates/zero-storage/src/outbox.rs".into(),
                "crates/imaginary/src/ghost.rs".into(),
            ],
            symbols: vec![],
        };
        let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
        let block = resolved.into_block();
        assert!(
            block.contains("crates/zero-storage/src/outbox.rs"),
            "real file must surface in block"
        );
        assert!(
            !block.contains("ghost.rs"),
            "missing file must not appear in block, got:\n{block}"
        );
    }

    #[tokio::test]
    async fn resolve_hints_empty_block_for_no_resolutions() {
        let workspace = StubWorkspace::default();
        let hints = ContextHints {
            paths: vec!["crates/nope/src/missing.rs".into()],
            symbols: vec!["NotAThing".into()],
        };
        let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
        assert!(resolved.is_empty());
        assert_eq!(resolved.into_block(), "");
    }

    #[tokio::test]
    async fn resolve_hints_honours_max_block_chars_by_dropping_bodies() {
        let big_body = "fn line() {}\n".repeat(200);
        let workspace = StubWorkspace::default()
            .with_file("crates/a/src/lib.rs", &big_body)
            .with_file("crates/b/src/lib.rs", &big_body);
        let hints = ContextHints {
            paths: vec![
                "crates/a/src/lib.rs".into(),
                "crates/b/src/lib.rs".into(),
            ],
            symbols: vec![],
        };
        let caps = ResolveCaps {
            max_block_chars: 400,
            ..default_caps()
        };
        let resolved = resolve_hints(&hints, &workspace, caps).await;
        let block = resolved.into_block();
        // Path list must survive even when bodies get dropped.
        assert!(block.contains("crates/a/src/lib.rs"));
        assert!(block.contains("crates/b/src/lib.rs"));
        // Block must be roughly within the budget (we stop dropping
        // once paths are body-less, so a small overshoot is fine for
        // the path list itself).
        assert!(
            block.len() <= 1000,
            "expected block trimmed near budget, got {} chars",
            block.len()
        );
    }

    #[tokio::test]
    async fn fs_workspace_reads_real_file_head() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hello.rs"),
            "line1\nline2\nline3\nline4\nline5\n",
        )
        .unwrap();
        let ws = FsWorkspace::new(dir.path());
        assert!(ws.exists("hello.rs").await);
        assert!(!ws.exists("missing.rs").await);
        let head = ws.read_file_head("hello.rs", 3).await.unwrap();
        assert_eq!(head, "line1\nline2\nline3");
    }

    #[tokio::test]
    async fn fs_workspace_grep_definition_finds_pub_fn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub struct Outbox;\n\nimpl Outbox {\n    pub fn enqueue(&self) {}\n}\n",
        )
        .unwrap();
        let ws = FsWorkspace::new(dir.path());
        let hits = ws.grep_definition("Outbox::enqueue", 3).await;
        assert!(!hits.is_empty(), "expected at least one hit");
        let first = &hits[0];
        assert_eq!(first.path, "src/lib.rs");
        assert!(first.text.contains("fn enqueue"));
    }
}
