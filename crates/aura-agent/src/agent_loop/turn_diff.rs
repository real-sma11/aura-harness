//! Per-iteration net file-op accumulator.
//!
//! Port of codex's `TurnDiffTracker`
//! ([codex-rs/core/src/turn_diff_tracker.rs:16](https://github.com/.../codex-rs/core/src/turn_diff_tracker.rs))
//! adapted to aura's `write_file` / `edit_file` / `delete_file` tool
//! surface (the codex tracker is shaped around its single-tool patch
//! envelope; aura keeps the granular write tools after Layer 0.4).
//!
//! Codex-parity note: the pre-parity build also tracked
//! `FailedWriteAttempt` records and `read_paths` here so the
//! out-of-loop continuation runtime could classify "no forward
//! motion" turns and surface tool-rejection snippets to the model.
//! The continuation runtime is gone (Codex trusts `EndTurn`), so
//! the failed-attempt channel is gone too. `read_paths` is retained
//! because `tool_pipeline::track_tool_effects` still records reads
//! into the session-scoped cache that drives in-session read-dedup.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// The net effect of a single iteration's write-tool calls on one file.
///
/// Later calls in the same iteration override earlier ones — e.g. a
/// `delete_file` after a `write_file` collapses to `Deleted`. Codex's
/// tracker does the same coalescing on its single-envelope patch ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FileOp {
    Created,
    Modified { bytes_written: usize },
    Deleted,
}

/// Net file-op map for the current iteration.
///
/// Reset at the top of every iteration by the agent loop.
///
/// The fields are `pub(crate)` so other agent-loop modules and their
/// tests can directly inspect the per-iteration accumulator without
/// needing accessor methods (the previous `is_empty` / `paths` /
/// `read_paths` getters were only ever consumed by tests in the
/// same crate and were carrying `#[allow(dead_code)]`).
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnDiff {
    pub(crate) writes: HashMap<PathBuf, FileOp>,
    /// Exploration-tool paths touched this iteration (`read_file`,
    /// `stat_file`, …). Still consumed by `tool_pipeline` to feed
    /// the session-scoped read-dedup cache.
    pub(crate) read_paths: HashSet<PathBuf>,
}

impl TurnDiff {
    /// Record a `write_file` on a path that did not exist before this
    /// iteration. Overrides any prior op on the same path (last-write-
    /// wins within an iteration).
    pub(crate) fn record_create(&mut self, path: PathBuf) {
        self.writes.insert(path, FileOp::Created);
    }

    /// Record a `write_file` / `edit_file` on an existing path. If the
    /// path was previously recorded as `Created` or `Modified` within
    /// this iteration, the byte count is summed onto the existing
    /// `Modified` entry (a `Created` entry is left as `Created` —
    /// the per-path coarse signal does not distinguish). `Deleted` is
    /// preserved (a delete-then-modify is unusual but the delete is
    /// the stronger signal).
    pub(crate) fn record_modify(&mut self, path: PathBuf, bytes: usize) {
        self.writes
            .entry(path)
            .and_modify(|op| {
                if let FileOp::Modified { bytes_written } = op {
                    *bytes_written = bytes_written.saturating_add(bytes);
                }
            })
            .or_insert(FileOp::Modified {
                bytes_written: bytes,
            });
    }

    /// Record a `delete_file`. Overrides any prior op on the same path
    /// — a create-then-delete in one iteration collapses to a deletion.
    pub(crate) fn record_delete(&mut self, path: PathBuf) {
        self.writes.insert(path, FileOp::Deleted);
    }

    /// Record a read-only tool touching `path` this iteration.
    pub(crate) fn record_read(&mut self, path: PathBuf) {
        self.read_paths.insert(path);
    }

    /// Clear all entries. Called at the top of each iteration by the
    /// agent loop so the diff scopes to the iteration just executed.
    pub(crate) fn reset(&mut self) {
        self.writes.clear();
        self.read_paths.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_diff_create_then_modify_keeps_modified_with_summed_bytes() {
        let mut diff = TurnDiff::default();
        let path = PathBuf::from("src/lib.rs");
        diff.record_modify(path.clone(), 100);
        diff.record_modify(path.clone(), 50);
        assert!(!diff.writes.is_empty());
        let op = diff.writes.get(&path).expect("entry must exist");
        assert_eq!(op, &FileOp::Modified { bytes_written: 150 });
    }

    #[test]
    fn turn_diff_create_followed_by_modify_keeps_created() {
        let mut diff = TurnDiff::default();
        let path = PathBuf::from("src/new.rs");
        diff.record_create(path.clone());
        diff.record_modify(path.clone(), 42);
        assert_eq!(diff.writes.get(&path), Some(&FileOp::Created));
    }

    #[test]
    fn turn_diff_delete_overrides_prior_op() {
        let mut diff = TurnDiff::default();
        let path = PathBuf::from("src/gone.rs");
        diff.record_create(path.clone());
        diff.record_modify(path.clone(), 99);
        diff.record_delete(path.clone());
        assert_eq!(diff.writes.get(&path), Some(&FileOp::Deleted));
    }

    #[test]
    fn turn_diff_reset_clears_all() {
        let mut diff = TurnDiff::default();
        diff.record_create(PathBuf::from("a.rs"));
        diff.record_modify(PathBuf::from("b.rs"), 10);
        diff.record_delete(PathBuf::from("c.rs"));
        diff.record_read(PathBuf::from("src/inbox.rs"));
        assert!(!diff.writes.is_empty());
        diff.reset();
        assert!(diff.writes.is_empty());
        assert!(diff.read_paths.is_empty());
    }

    #[test]
    fn turn_diff_records_read_paths() {
        let mut diff = TurnDiff::default();
        diff.record_read(PathBuf::from("crates/foo/src/inbox.rs"));
        assert_eq!(diff.read_paths.len(), 1);
        assert!(diff
            .read_paths
            .contains(&PathBuf::from("crates/foo/src/inbox.rs")));
    }

    #[test]
    fn turn_diff_is_empty_after_default() {
        let diff = TurnDiff::default();
        assert!(diff.writes.is_empty());
        assert!(diff.read_paths.is_empty());
    }
}
