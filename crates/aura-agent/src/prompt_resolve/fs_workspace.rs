//! Real-filesystem implementation of [`super::WorkspaceReader`],
//! rooted at a project folder path. Uses `tokio::fs` for reads and a
//! synchronous `walkdir + regex` pass on a blocking pool for
//! definition grep so the tokio reactor never blocks on a large
//! workspace walk.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use walkdir::WalkDir;

use super::{SymbolHit, WorkspaceReader};

/// Real-filesystem implementation of the workspace reader.
pub struct FsWorkspace {
    root: PathBuf,
}

impl FsWorkspace {
    /// Construct an `FsWorkspace` rooted at `root`.
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
        tokio::fs::metadata(&full).await.is_ok_and(|m| m.is_file())
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

    async fn discover_module_paths(&self, module: &str, max_hits: usize) -> Vec<String> {
        let root = self.root.clone();
        let module = module.to_string();
        tokio::task::spawn_blocking(move || {
            discover_module_paths_blocking(&root, &module, max_hits)
        })
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

fn discover_module_paths_blocking(root: &Path, module: &str, max_hits: usize) -> Vec<String> {
    if max_hits == 0 || module.is_empty() {
        return Vec::new();
    }
    let target = format!("{module}.rs");
    let mut paths = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                return !SKIP_DIRS.contains(&name.as_ref());
            }
            true
        })
    {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if file_name != target {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        if !rel.contains("/src/") {
            continue;
        }
        if !paths.contains(&rel) {
            paths.push(rel);
        }
        if paths.len() >= max_hits {
            break;
        }
    }
    paths
}
