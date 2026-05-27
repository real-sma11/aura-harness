//! Tool-result memoization caches owned by [`super::LoopState`].
//!
//! Carved out of `agent_loop/mod.rs` during the Phase 3 god-module
//! split so the cache invariants live next to their helpers in
//! `super::tool_execution` / `super::search_cache` rather than at
//! module-root scope. The types stay `pub(crate)` so the existing
//! call sites inside `agent_loop::*` keep working without elevating
//! visibility outside the crate (Rule 3.1).

use std::collections::HashMap;

/// Tool-result memoization shared by [`super::tool_execution`] and the
/// fuzzy-search lookup in [`super::search_cache`].
///
/// The `exact` / `fuzzy` maps remain the primary read-side hits.
/// Phase 1 of the reread-efficiency plan adds two extra indices:
///
/// * [`Self::read_file_by_path`] is a per-path range index over
///   `read_file` results. On an exact-key miss for `read_file`, the
///   loop consults this vec for a previously-cached window that
///   *contains* the requested `start_line..=end_line`, and slices it
///   in-memory instead of re-running the tool. The slicing layer is
///   intentionally conservative: it does not touch the disk, and it
///   re-uses the superset entry's `content_hash` so downstream
///   compaction dedup can fold the subset response back into the
///   original read.
///
/// Path-scoped invalidation: on a successful write, `update_cache`
/// no longer wipes both maps wholesale. It drops only the cache
/// entries whose path equals, parents, or descends the written path;
/// `search_code` / `find_files` entries still invalidate
/// workspace-wide because their results are not path-scoped.
#[derive(Default)]
pub(crate) struct ToolResultCache {
    /// Exact-key cache: `tool_name + canonical_input_json`.
    pub(crate) exact: HashMap<String, String>,
    /// Secondary, normalized index for `search_code` / `find_files`
    /// that collapses alternation-order and trivial whitespace
    /// variants. Populated alongside `exact` in `update_cache`;
    /// consulted only on a miss of the exact key. Cleared together
    /// with the workspace-global slice of `exact` on any successful
    /// write so the "write invalidates search" invariant is preserved.
    pub(crate) fuzzy: HashMap<String, String>,
    /// Per-path range index over `read_file` results. Keyed by the
    /// canonical (forward-slash, no trailing slash, `./` stripped)
    /// path string. Each entry records the window the call returned
    /// plus the rendered tool output so a later subset request can be
    /// served without disk I/O.
    pub(crate) read_file_by_path: HashMap<String, Vec<ReadRangeEntry>>,
}

/// One cached `read_file` result, indexed by path in
/// [`ToolResultCache::read_file_by_path`].
///
/// We store the rendered tool output (the exact bytes the model saw).
/// Slicing for a subset request lifts lines out of `rendered` by
/// their leading `{:>6}|` line-number prefix — no `fs::read` call, no
/// second pass through the tool. Whole-file entries (`start_line` and
/// `end_line` both `None`) carry the raw bytes in `rendered` and are
/// re-rendered in memory on demand.
#[derive(Debug, Clone)]
pub(crate) struct ReadRangeEntry {
    pub(crate) start_line: Option<usize>,
    pub(crate) end_line: Option<usize>,
    pub(crate) rendered: String,
}
