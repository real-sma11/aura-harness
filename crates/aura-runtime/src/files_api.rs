//! Shared directory-walking and file-read helpers used by both the
//! `aura-runtime` HTTP router and the embedded TUI-mode API server in the
//! `aura` binary crate.
//!
//! The two callers had drifted into near-duplicate implementations of
//! `/api/files` and `/api/read-file` — identical `IGNORED_DIRS`,
//! identical `MAX_READ_BYTES` cap, only slightly different walkers.
//! Phase 3 consolidates that here so a change to the ignore list or
//! the read cap only needs to land in one place.
//!
//! Path resolution is still caller-specific: `aura-runtime` goes through
//! [`crate::config::NodeConfig::resolve_allowed_path`] and serializes
//! entries with workspace-relative, forward-slash normalized paths;
//! the TUI server goes through [`aura_tools::Sandbox`] and serializes
//! absolute paths. Those differences are deliberate (the two consumers
//! have different JSON contracts) so this module stays one level below
//! them: it owns the walk and the read, callers own the path
//! canonicalisation and the JSON shape.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use tracing::{debug, warn};

/// Directory names the walker unconditionally skips.
///
/// These are VCS metadata, package-manager caches, and build outputs.
/// They are never interesting to an LLM browsing the workspace and
/// descending into them inflates the response by O(100k) entries
/// without the operator learning anything useful.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".next",
    "dist",
    "build",
    ".svn",
    ".hg",
    "vendor",
];

/// Maximum bytes `read_file_capped` will buffer before tripping the
/// "too large" signal.
///
/// Caps accidental `cat`s of huge files and, more importantly, denies
/// an OOM vector where a symlink / junction points at a pseudo-file
/// such as `/dev/zero` so a naïve `read_to_end` would never return.
pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

/// Absolute max depth a walker may descend, regardless of caller
/// input. Stops a `depth=9999` query from running the process out of
/// file descriptors.
pub const MAX_WALK_DEPTH: usize = 20;

/// A single entry in the walked tree.
///
/// Paths are reported as the absolute `PathBuf` discovered by the walk
/// (i.e. `base.join(relative)`). Callers that want workspace-relative
/// or forward-slash normalised paths should convert as part of their
/// DTO mapping — keeping this struct as the walker's raw output means
/// we don't force one path convention onto every consumer.
#[derive(Debug, Clone)]
pub struct WalkedEntry {
    pub name: String,
    pub abs_path: PathBuf,
    pub is_dir: bool,
    pub children: Option<Vec<WalkedEntry>>,
}

/// Walk `start` up to `max_depth` directories deep, returning a sorted
/// tree of non-hidden, non-ignored entries.
///
/// # Safety properties
///
/// * Hidden entries (leading `.`) are skipped.
/// * Directories in [`IGNORED_DIRS`] are skipped.
/// * The walk is driven by an explicit stack — no async recursion, so
///   stack depth is bounded by Tokio's task budget rather than the
///   filesystem shape.
/// * If `workspace_root` is `Some`, every visited directory is
///   canonicalised (symlinks collapsed to their real target) and
///   skipped unless its canonical path is a descendant of the root.
///   This is defence-in-depth against a mid-walk symlink that points
///   at `/etc` or similar — the sandbox's entry-point check alone is
///   not enough once the walker starts following links.
/// * Even without `workspace_root`, a visited-set of canonical paths
///   breaks cycles (`a -> b -> a` symlinks).
///
/// `max_depth` is clamped to [`MAX_WALK_DEPTH`] regardless of the
/// value supplied.
pub async fn walk_directory(
    start: &Path,
    workspace_root: Option<&Path>,
    max_depth: usize,
) -> Vec<WalkedEntry> {
    let max_depth = max_depth.min(MAX_WALK_DEPTH);

    // Phase 1: async iterative walk. For each directory we visit we
    // record its ordered children (dirs before files, alphabetical
    // within each group) in a flat map keyed by the directory's own
    // absolute path.
    let mut contents: HashMap<PathBuf, Vec<(String, bool, PathBuf)>> = HashMap::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(start.to_path_buf(), 0)];

    while let Some((path, depth)) = stack.pop() {
        if depth >= max_depth {
            continue;
        }
        // Canonicalize per-step so intermediate symlinks collapse to
        // their real target. Failed canonicalize (broken symlink,
        // permission denied, missing) => skip rather than abort the
        // whole walk.
        let canonical = match std::fs::canonicalize(&path) {
            Ok(c) => strip_unc_prefix(&c),
            Err(_) => {
                warn!(path = %path.display(), "failed to canonicalize during walk");
                continue;
            }
        };
        if let Some(root) = workspace_root {
            if !canonical.starts_with(root) {
                debug!(
                    path = %path.display(),
                    canonical = %canonical.display(),
                    "walk_directory skipping path that canonicalises outside workspace"
                );
                continue;
            }
        }
        if !visited.insert(canonical) {
            // Already walked via some other name (symlink, junction) —
            // skip so we don't inflate the contents map on cycles.
            continue;
        }

        let mut read_dir = match tokio::fs::read_dir(&path).await {
            Ok(rd) => rd,
            Err(_) => {
                warn!(path = %path.display(), "failed to read directory during walk");
                continue;
            }
        };

        let mut dirs: Vec<(String, PathBuf)> = Vec::new();
        let mut files: Vec<(String, PathBuf)> = Vec::new();

        loop {
            match read_dir.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let entry_path = entry.path();
                    let is_dir = match entry.file_type().await {
                        Ok(ft) => ft.is_dir(),
                        Err(_) => false,
                    };
                    if is_dir {
                        if !IGNORED_DIRS.contains(&name.as_str()) {
                            dirs.push((name, entry_path));
                        }
                    } else {
                        files.push((name, entry_path));
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        dirs.sort_by(|a, b| a.0.cmp(&b.0));
        files.sort_by(|a, b| a.0.cmp(&b.0));

        let mut rel_contents: Vec<(String, bool, PathBuf)> =
            Vec::with_capacity(dirs.len() + files.len());
        for (name, entry_path) in &dirs {
            rel_contents.push((name.clone(), true, entry_path.clone()));
        }
        for (name, entry_path) in files {
            rel_contents.push((name, false, entry_path));
        }
        contents.insert(path, rel_contents);

        for (_, entry_path) in dirs {
            stack.push((entry_path, depth + 1));
        }
    }

    // Phase 2: pure in-memory tree assembly bounded by `max_depth`
    // (<= MAX_WALK_DEPTH), so sync recursion is safe and keeps the
    // shape simple.
    fn assemble(
        path: &Path,
        contents: &HashMap<PathBuf, Vec<(String, bool, PathBuf)>>,
    ) -> Vec<WalkedEntry> {
        let Some(children) = contents.get(path) else {
            return Vec::new();
        };
        children
            .iter()
            .map(|(name, is_dir, entry_path)| {
                let kids = if *is_dir {
                    Some(assemble(entry_path, contents))
                } else {
                    None
                };
                WalkedEntry {
                    name: name.clone(),
                    abs_path: entry_path.clone(),
                    is_dir: *is_dir,
                    children: kids,
                }
            })
            .collect()
    }

    assemble(start, &contents)
}

/// Outcome of a capped-read.
#[derive(Debug)]
pub enum ReadOutcome {
    /// File fit within `max_bytes`; `bytes` holds the full contents.
    Ok { bytes: Vec<u8> },
    /// File exceeded `max_bytes`. The handler should reply `413`.
    TooLarge { max_bytes: u64 },
}

/// Errors produced by [`read_file_capped`]. Caller translates to HTTP.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("failed to open file: {0}")]
    Open(std::io::Error),
    #[error("failed to read file: {0}")]
    Read(std::io::Error),
}

/// Read `path` into memory, refusing to buffer more than `max_bytes`.
///
/// Uses `AsyncReadExt::take(max_bytes + 1)` so we can distinguish
/// "exactly at the cap" from "over the cap" without ever holding more
/// than `max_bytes + 1` bytes in memory. `read_to_end` / `read_to_string`
/// would have allocated for the full file up front — the whole point
/// of this helper is to deny that.
pub async fn read_file_capped(path: &Path, max_bytes: u64) -> Result<ReadOutcome, ReadError> {
    let file = tokio::fs::File::open(path).await.map_err(ReadError::Open)?;
    let mut buf: Vec<u8> = Vec::new();
    let mut limited = file.take(max_bytes + 1);
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(ReadError::Read)?;
    if buf.len() as u64 > max_bytes {
        return Ok(ReadOutcome::TooLarge { max_bytes });
    }
    Ok(ReadOutcome::Ok { bytes: buf })
}

/// Strip the `\\?\` verbatim prefix that Windows `canonicalize` adds
/// so walk-time canonical paths compare cleanly against a workspace
/// root that has already had its own prefix stripped. No-op on
/// non-Windows targets.
fn strip_unc_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walk_respects_ignore_list() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::write(root.join("src").join("lib.rs"), "").unwrap();
        std::fs::write(root.join("node_modules").join("junk.js"), "").unwrap();

        let entries = walk_directory(root, None, 3).await;
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src"]);
    }

    #[tokio::test]
    async fn walk_sorts_dirs_before_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::create_dir(root.join("z_dir")).unwrap();
        let entries = walk_directory(root, None, 3).await;
        assert_eq!(entries[0].name, "z_dir");
        assert_eq!(entries[1].name, "a.txt");
    }

    #[tokio::test]
    async fn read_capped_returns_small_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("hi.txt");
        std::fs::write(&path, "hello").unwrap();
        match read_file_capped(&path, 1024).await.unwrap() {
            ReadOutcome::Ok { bytes } => assert_eq!(bytes, b"hello"),
            ReadOutcome::TooLarge { .. } => panic!("should not be too large"),
        }
    }

    #[tokio::test]
    async fn read_capped_rejects_oversize() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, vec![b'A'; 100]).unwrap();
        match read_file_capped(&path, 50).await.unwrap() {
            ReadOutcome::TooLarge { max_bytes } => assert_eq!(max_bytes, 50),
            ReadOutcome::Ok { .. } => panic!("should have been too large"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn walk_breaks_symlink_loops() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("marker.txt"), "ok").unwrap();
        std::os::unix::fs::symlink(&root, child.join("loop")).unwrap();

        let entries = walk_directory(&root, Some(&root), 20).await;

        fn count(entries: &[WalkedEntry]) -> usize {
            entries
                .iter()
                .map(|e| 1 + e.children.as_deref().map_or(0, count))
                .sum()
        }
        let n = count(&entries);
        assert!(n < 50, "symlink loop inflated tree to {n} nodes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn walk_refuses_escaping_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        std::fs::write(outside_root.join("secret.txt"), "top-secret").unwrap();
        std::os::unix::fs::symlink(&outside_root, root.join("escape")).unwrap();

        let entries = walk_directory(&root, Some(&root), 5).await;

        fn has_name(entries: &[WalkedEntry], needle: &str) -> bool {
            entries.iter().any(|e| {
                e.name == needle || e.children.as_deref().is_some_and(|c| has_name(c, needle))
            })
        }
        assert!(!has_name(&entries, "secret.txt"));
    }
}
