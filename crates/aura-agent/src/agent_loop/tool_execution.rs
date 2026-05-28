//! Tool result caching primitives and `truncate_preview` helper.
//!
//! Phase 4 collapsed the dispatch / execute / emit / push pipeline
//! that used to live here into the unified
//! [`super::tool_pipeline::process_tool_results`] entry point. This
//! module now owns only the per-run [`ToolResultCache`] machinery
//! ([`split_cached`] / [`update_cache`] / per-path range index) and
//! the small text-truncation helper used by error logging.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use aura_config::{tool_result_cache_key, CACHEABLE_TOOLS};
use serde_json::Value;
use tracing::debug;

use crate::helpers;
use crate::types::{ToolCallInfo, ToolCallResult};

use super::search_cache::normalized_search_key;
use super::{ReadRangeEntry, ToolResultCache};

fn is_cacheable(tool_name: &str) -> bool {
    CACHEABLE_TOOLS.contains(&tool_name)
}

/// Sanitise a tool error body for inline embedding in a `tracing` log
/// field: collapse whitespace, drop control characters, replace inner
/// double quotes (which would otherwise break naive `key="value"`
/// parsers like `infra/evals/external/bin/follow-harness-log.mjs`),
/// and clip to `limit` characters with an ASCII marker.
pub(super) fn truncate_preview(content: &str, limit: usize) -> String {
    let collapsed: String = content
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = collapsed
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "'");
    if trimmed.chars().count() <= limit {
        trimmed
    } else {
        let head: String = trimmed.chars().take(limit).collect();
        format!("{head}...")
    }
}

pub(super) fn split_cached(
    tool_calls: &[ToolCallInfo],
    cache: &ToolResultCache,
) -> (Vec<ToolCallResult>, Vec<ToolCallInfo>) {
    let mut cached = Vec::new();
    let mut uncached = Vec::new();

    for tc in tool_calls {
        if !is_cacheable(&tc.name) {
            uncached.push(tc.clone());
            continue;
        }

        let exact_key = tool_result_cache_key(&tc.name, &tc.input);
        if let Some(hit) = cache.exact.get(&exact_key) {
            debug!(
                tool_use_id = %tc.id,
                tool_name = %tc.name,
                source = "cache:exact",
                "Tool call satisfied from cache"
            );
            cached.push(cached_tool_result(tc, hit.clone()));
            continue;
        }

        // Phase 1: per-path range fallback for `read_file`. On an
        // exact-key miss, walk the per-path vec for a superset window
        // and slice it in-memory. We do NOT consult the disk here —
        // the slice is derived purely from the cached rendered bytes.
        if tc.name == "read_file" {
            if let Some(hit) = range_cache_lookup(cache, &tc.input) {
                debug!(
                    tool_use_id = %tc.id,
                    tool_name = %tc.name,
                    source = "cache:range",
                    "Tool call satisfied from range cache"
                );
                cached.push(cached_tool_result(tc, hit));
                continue;
            }
        }

        // Fall back to the normalized (fuzzy) index for
        // `search_code` / `find_files` only. Other cacheable tools
        // (`read_file`, `list_files`, `stat_file`) stay exact-only
        // because their keys already describe a single resource.
        if let Some(fkey) = normalized_search_key(&tc.name, &tc.input) {
            if let Some(hit) = cache.fuzzy.get(&fkey) {
                debug!(
                    tool_use_id = %tc.id,
                    tool_name = %tc.name,
                    source = "cache:fuzzy",
                    "Tool call satisfied from fuzzy cache"
                );
                cached.push(cached_tool_result(tc, hit.clone()));
                continue;
            }
        }

        uncached.push(tc.clone());
    }

    (cached, uncached)
}

fn cached_tool_result(call: &ToolCallInfo, content: String) -> ToolCallResult {
    ToolCallResult {
        tool_use_id: call.id.clone(),
        content,
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    }
}

pub(super) fn update_cache(
    cache: &mut ToolResultCache,
    uncached: &[ToolCallInfo],
    executed: &[ToolCallResult],
) {
    let mut write_paths: Vec<String> = Vec::new();
    for tc in uncached {
        if !helpers::is_write_tool(&tc.name) {
            continue;
        }
        let succeeded = executed
            .iter()
            .any(|r| r.tool_use_id == tc.id && !r.is_error);
        if !succeeded {
            continue;
        }
        if let Some(path) = extract_path_arg(&tc.input) {
            write_paths.push(path);
        }
    }

    if !write_paths.is_empty() {
        // Drop only the path-scoped cache entries that overlap one of
        // the written paths. `search_code` / `find_files` entries
        // stay workspace-global because their results aggregate
        // across the tree.
        invalidate_on_writes(cache, &write_paths);
    }

    for r in executed {
        if let Some(tc) = uncached.iter().find(|t| t.id == r.tool_use_id) {
            if is_cacheable(&tc.name) && !r.is_error {
                let key = tool_result_cache_key(&tc.name, &tc.input);
                cache.exact.insert(key, r.content.clone());
                if let Some(fkey) = normalized_search_key(&tc.name, &tc.input) {
                    cache.fuzzy.insert(fkey, r.content.clone());
                }
                if tc.name == "read_file" {
                    insert_read_range_entry(cache, &tc.input, &r.content);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 1: range-aware `read_file` cache
// ---------------------------------------------------------------------------

/// Canonicalise a tool-reported path so backslashes, leading `./`, and
/// trailing slashes don't fragment the per-path indices.
///
/// Intentionally kept lexical (no `fs::canonicalize`): the harness
/// cache layer never touches the disk and a relative path is the only
/// thing the model ever produces here.
fn canonical_tool_path(path: &str) -> String {
    let s = path.replace('\\', "/");
    let s = s.trim_start_matches("./");
    s.trim_end_matches('/').to_string()
}

/// Extract the `path` string from a tool's input arguments and
/// canonicalise it. Returns `None` when the tool has no `path` arg or
/// it's empty.
fn extract_path_arg(input: &Value) -> Option<String> {
    let raw = input.get("path").and_then(Value::as_str)?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(canonical_tool_path(raw))
}

/// Pull `(canonical_path, start_line, end_line)` out of a `read_file`
/// invocation's input JSON. `start_line` / `end_line` are 1-indexed
/// and inclusive, matching `aura_tools::fs_tools::read::fs_read`.
fn read_file_window(input: &Value) -> Option<(String, Option<usize>, Option<usize>)> {
    let path = extract_path_arg(input)?;
    let start = input
        .get("start_line")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let end = input
        .get("end_line")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    Some((path, start, end))
}

/// Returns `true` when the cached `entry` window contains the
/// `requested` (start, end). `None` boundaries on the cached entry
/// mean "no bound on that side" (whole-file). `None` boundaries on
/// the request collapse to `start=1` / `end=usize::MAX` so a
/// whole-file request only matches a whole-file cached entry.
fn entry_covers(entry: &ReadRangeEntry, req_start: Option<usize>, req_end: Option<usize>) -> bool {
    if entry.start_line.is_none() && entry.end_line.is_none() {
        // Whole-file cached → covers any window.
        return true;
    }
    let cached_start = entry.start_line.unwrap_or(1);
    let cached_end = entry.end_line.unwrap_or(usize::MAX);
    let want_start = req_start.unwrap_or(1).max(1);
    let want_end = req_end.unwrap_or(usize::MAX);
    cached_start <= want_start && want_end <= cached_end
}

/// Look up the per-path range index for a `read_file` request. On a
/// hit, returns the rendered tool output already sliced to the
/// requested window. On a miss, returns `None`.
fn range_cache_lookup(cache: &ToolResultCache, input: &Value) -> Option<String> {
    let (path, req_start, req_end) = read_file_window(input)?;
    let entries = cache.read_file_by_path.get(&path)?;
    for entry in entries {
        if !entry_covers(entry, req_start, req_end) {
            continue;
        }
        if let Some(sliced) = slice_entry(entry, req_start, req_end) {
            return Some(sliced);
        }
    }
    None
}

/// Slice a cached `read_file` entry down to the requested window.
///
/// Three paths, in priority order:
///
/// 1. Cached entry is `(Some(s), Some(e))` — its `rendered` text is
///    line-numbered. We walk the lines (split on `\n`), trust the
///    leading `{:>6}|` prefix, and pick the rows whose original line
///    number falls inside the request.
/// 2. Cached entry is whole-file (`(None, None)`) — its `rendered`
///    text is the raw file bytes (utf-8 lossy). We split on `\n` and
///    re-render the requested window with the standard
///    `{:>6}|{content}` prefix to mirror what `fs_read` would emit.
/// 3. Anything else (half-bounded cached windows) is rejected by
///    `entry_covers` above, so this slot is unreachable today; the
///    final `None` keeps the function total.
///
/// On any truncation marker in the rendered text we bail out (return
/// `None`) so the caller falls through to a fresh `fs_read` rather
/// than serving a sliced truncated body whose line numbering may not
/// match the original file.
fn slice_entry(
    entry: &ReadRangeEntry,
    req_start: Option<usize>,
    req_end: Option<usize>,
) -> Option<String> {
    const TRUNCATION_MARKER: &str = "\n... [truncated";
    if entry.rendered.contains(TRUNCATION_MARKER) {
        return None;
    }
    let want_start = req_start.unwrap_or(1).max(1);
    let want_end = req_end.unwrap_or(usize::MAX);
    if want_end < want_start {
        return None;
    }

    match (entry.start_line, entry.end_line) {
        (Some(_), Some(_)) | (Some(_), None) | (None, Some(_)) => {
            slice_line_numbered(&entry.rendered, want_start, want_end)
        }
        (None, None) => slice_raw_to_line_numbered(&entry.rendered, want_start, want_end),
    }
}

fn slice_line_numbered(rendered: &str, want_start: usize, want_end: usize) -> Option<String> {
    let kept: Vec<&str> = rendered
        .split('\n')
        .filter_map(|row| {
            let (prefix, _rest) = row.split_once('|')?;
            let n: usize = prefix.trim().parse().ok()?;
            (n >= want_start && n <= want_end).then_some(row)
        })
        .collect();
    if kept.is_empty() {
        return None;
    }
    Some(kept.join("\n"))
}

fn slice_raw_to_line_numbered(
    rendered: &str,
    want_start: usize,
    want_end: usize,
) -> Option<String> {
    let lines: Vec<&str> = rendered.lines().collect();
    let total = lines.len();
    if total == 0 || want_start > total {
        return None;
    }
    let end = want_end.min(total);
    let start = want_start.max(1);
    if end < start {
        return None;
    }
    let body: Vec<String> = lines[(start - 1)..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}|{}", start + i, line))
        .collect();
    Some(body.join("\n"))
}

/// Compute the same `content_hash` that `aura_tools::fs_tools::read`
/// stamps onto its `read_file` metadata. We re-derive it here because
/// the agent-loop cache only receives the rendered `String` payload,
/// not the upstream `ToolResult` metadata map. Keeping a private copy
/// of the hash avoids plumbing metadata through `ToolCallResult`
/// (which would touch every executor and test fixture in the tree).
#[allow(dead_code)]
pub(crate) fn content_hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Store a freshly-executed `read_file` result into the per-path
/// range index. Any pre-existing entry that is fully covered by the
/// new one is pruned so the vec doesn't grow without bound.
fn insert_read_range_entry(cache: &mut ToolResultCache, input: &Value, rendered: &str) {
    let Some((path, start_line, end_line)) = read_file_window(input) else {
        return;
    };
    let new_entry = ReadRangeEntry {
        start_line,
        end_line,
        rendered: rendered.to_string(),
    };
    let entries = cache.read_file_by_path.entry(path).or_default();
    entries.retain(|existing| !entry_strictly_covers(&new_entry, existing));
    entries.push(new_entry);
}

/// `outer` covers `inner` for pruning purposes when their windows
/// overlap in the same direction. Whole-file (`None`, `None`)
/// dominates everything; otherwise both ends must be at least as
/// permissive.
fn entry_strictly_covers(outer: &ReadRangeEntry, inner: &ReadRangeEntry) -> bool {
    if outer.start_line.is_none() && outer.end_line.is_none() {
        return true;
    }
    let outer_start = outer.start_line.unwrap_or(1);
    let outer_end = outer.end_line.unwrap_or(usize::MAX);
    let inner_start = inner.start_line.unwrap_or(1);
    let inner_end = inner.end_line.unwrap_or(usize::MAX);
    outer_start <= inner_start && inner_end <= outer_end
}

// ---------------------------------------------------------------------------
// Phase 1B: path-scoped invalidation
// ---------------------------------------------------------------------------

/// Drop cache entries that overlap one of `write_paths`. The exact
/// cache is filtered on a per-tool basis:
///
/// * `search_code` / `find_files`: cleared workspace-wide (current
///   behaviour) — their results aggregate across the tree so any
///   write may have invalidated them.
/// * `read_file` / `list_files` / `stat_file`: dropped only when the
///   entry's `path` argument overlaps one of the written paths.
///
/// The `fuzzy` map is cleared workspace-wide because every entry in
/// it is by definition a `search_code` / `find_files` result.
///
/// The per-path range index is filtered key-by-key.
fn invalidate_on_writes(cache: &mut ToolResultCache, write_paths: &[String]) {
    let exact_keys: Vec<String> = cache.exact.keys().cloned().collect();
    for key in exact_keys {
        let Some((tool_name, canonical_input)) = key.split_once('\0') else {
            continue;
        };
        let drop = if tool_name == "search_code" || tool_name == "find_files" {
            true
        } else {
            cached_input_path(canonical_input)
                .is_some_and(|p| write_paths.iter().any(|w| paths_overlap(w, &p)))
        };
        if drop {
            cache.exact.remove(&key);
        }
    }

    cache.fuzzy.clear();

    cache
        .read_file_by_path
        .retain(|path, _| !write_paths.iter().any(|w| paths_overlap(w, path)));
}

/// Decode the path arg out of the canonical JSON suffix of an exact
/// cache key. Returns `None` for tools whose input isn't a JSON
/// object or doesn't carry a `path` field.
fn cached_input_path(canonical_input: &str) -> Option<String> {
    let value: Value = serde_json::from_str(canonical_input).ok()?;
    extract_path_arg(&value)
}

/// Path-overlap predicate: equal, ancestor, or descendant. Empty
/// strings short-circuit to `false` so a missing path on either side
/// never invalidates anything by accident.
fn paths_overlap(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let pa = Path::new(a);
    let pb = Path::new(b);
    pa.starts_with(pb) || pb.starts_with(pa)
}
