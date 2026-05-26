//! Per-iteration net file-op accumulator.
//!
//! Port of codex's `TurnDiffTracker`
//! ([codex-rs/core/src/turn_diff_tracker.rs:16](https://github.com/.../codex-rs/core/src/turn_diff_tracker.rs))
//! adapted to aura's `write_file` / `edit_file` / `delete_file` tool
//! surface (the codex tracker is shaped around its single-tool patch
//! envelope; aura keeps the granular write tools after Layer 0.4).
//!
//! Phase 1.A foundation for [`super::continuation`]: the continuation
//! runtime needs path-level data — not just a `had_any_file_write:
//! bool` — to detect "no forward motion this turn" and to compute a
//! blocker_signature for the codex-style blocked-after-3 audit.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
/// Reset at the top of every iteration by the agent loop; consulted by
/// `continuation::ContinuationState::on_iteration_end` to decide
/// whether the turn produced forward motion.
#[derive(Debug, Default, Clone)]
pub(crate) struct TurnDiff {
    writes: HashMap<PathBuf, FileOp>,
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
    /// is_empty() and the per-path coarse signal don't distinguish).
    /// `Deleted` is preserved (a delete-then-modify is unusual but
    /// the delete is the stronger signal).
    pub(crate) fn record_modify(&mut self, path: PathBuf, bytes: usize) {
        self.writes
            .entry(path)
            .and_modify(|op| {
                if let FileOp::Modified { bytes_written } = op {
                    *bytes_written = bytes_written.saturating_add(bytes);
                }
            })
            .or_insert(FileOp::Modified { bytes_written: bytes });
    }

    /// Record a `delete_file`. Overrides any prior op on the same path
    /// — a create-then-delete in one iteration collapses to a deletion.
    pub(crate) fn record_delete(&mut self, path: PathBuf) {
        self.writes.insert(path, FileOp::Deleted);
    }

    /// Returns true when no write/edit/delete landed this iteration.
    /// This is the per-iteration "no forward motion" signal consumed
    /// by `ContinuationState::on_iteration_end` (Phase 1.B).
    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// Iterate over the paths touched this iteration. Reserved for the
    /// blocker_signature computation in a future Phase 1.B follow-up
    /// (the integration is currently best-effort).
    #[allow(dead_code)]
    pub(crate) fn paths(&self) -> impl Iterator<Item = &Path> {
        self.writes.keys().map(PathBuf::as_path)
    }

    /// Clear all entries. Called at the top of each iteration by the
    /// agent loop so the diff scopes to the iteration just executed.
    pub(crate) fn reset(&mut self) {
        self.writes.clear();
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
        assert!(!diff.is_empty());
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
        assert!(!diff.is_empty());
        diff.reset();
        assert!(diff.is_empty());
        assert_eq!(diff.paths().count(), 0);
    }

    #[test]
    fn turn_diff_is_empty_after_default() {
        let diff = TurnDiff::default();
        assert!(diff.is_empty());
    }

    #[test]
    fn turn_diff_paths_iterates_all_recorded() {
        let mut diff = TurnDiff::default();
        diff.record_create(PathBuf::from("a.rs"));
        diff.record_modify(PathBuf::from("b.rs"), 1);
        let mut paths: Vec<_> = diff.paths().map(Path::to_path_buf).collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
    }
}
