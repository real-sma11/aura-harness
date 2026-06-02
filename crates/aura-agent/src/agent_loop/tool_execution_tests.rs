use aura_model_reasoner::ContentBlock;

use crate::types::ToolCallInfo;
use crate::types::ToolCallResult;
use aura_config::tool_result_cache_key;

use super::search_cache::normalized_search_key;
use super::tool_execution::{split_cached, truncate_preview, update_cache};
use super::tool_pipeline::push_tool_result_message;
use super::ToolResultCache;

#[test]
fn tool_results_are_emitted_before_context_texts() {
    let mut messages = Vec::new();
    let results = vec![
        ToolCallResult {
            tool_use_id: "tool_1".to_string(),
            content: "ok 1".to_string(),
            is_error: false,
            kind: aura_core_types::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        },
        ToolCallResult {
            tool_use_id: "tool_2".to_string(),
            content: "ok 2".to_string(),
            is_error: true,
            kind: aura_core_types::ToolResultKind::AgentError,
            stop_loop: false,
            file_changes: Vec::new(),
            image: None,
        },
    ];
    let context = vec!["Build check failed".to_string()];

    push_tool_result_message(&mut messages, results, context);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, aura_model_reasoner::Role::User);
    assert!(matches!(
        messages[0].content.first(),
        Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == "tool_1"
    ));
    assert!(matches!(
        messages[0].content.get(1),
        Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == "tool_2"
    ));
    assert!(matches!(
        messages[0].content.get(2),
        Some(ContentBlock::Text { text }) if text == "Build check failed"
    ));
}

/// Cached read hits now reinsert the cache content verbatim — the
/// silent cache-result-shaping path was removed because it was
/// rewriting `read_file` results without telling the model. This test
/// pins the new contract: the same long content goes back into the
/// transcript byte-for-byte.
#[test]
fn cached_read_hits_reinsert_content_verbatim() {
    let call = ToolCallInfo {
        id: "tool_1".to_string(),
        name: "read_file".to_string(),
        input: serde_json::json!({"path": "src/lib.rs"}),
    };
    let mut cache = ToolResultCache::default();
    let long_content = "a".repeat(9_000);
    cache.exact.insert(
        tool_result_cache_key(&call.name, &call.input),
        long_content.clone(),
    );

    let (cached, uncached) = split_cached(&[call], &cache);

    assert!(uncached.is_empty());
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].content, long_content);
}

#[test]
fn fuzzy_cache_hits_after_alternation_reorder() {
    // Seed the cache by running update_cache with a successful
    // search_code result for pattern "pub fn generate|NeuralKey".
    let seed = ToolCallInfo {
        id: "tool_seed".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "pub fn generate|NeuralKey"}),
    };
    let seed_result = ToolCallResult {
        tool_use_id: "tool_seed".to_string(),
        content: "seed-hits".to_string(),
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    let mut cache = ToolResultCache::default();
    update_cache(
        &mut cache,
        std::slice::from_ref(&seed),
        std::slice::from_ref(&seed_result),
    );
    assert!(!cache.exact.is_empty(), "exact cache should be populated");
    assert!(!cache.fuzzy.is_empty(), "fuzzy cache should be populated");

    // Now a later call with the alternation terms in a different order
    // — it should MISS the exact cache but HIT the fuzzy cache.
    let reordered = ToolCallInfo {
        id: "tool_query".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "NeuralKey|pub fn generate"}),
    };

    assert!(
        !cache
            .exact
            .contains_key(&tool_result_cache_key(&reordered.name, &reordered.input)),
        "exact key should not match the reordered alternation"
    );

    let (cached, uncached) = split_cached(&[reordered], &cache);
    assert!(
        uncached.is_empty(),
        "fuzzy cache should satisfy the reordered query without executing"
    );
    assert_eq!(cached.len(), 1);
    assert_eq!(cached[0].tool_use_id, "tool_query");
    assert!(!cached[0].is_error);
}

#[test]
fn write_clears_both_caches() {
    // Seed both caches with a search_code result.
    let seed = ToolCallInfo {
        id: "tool_s".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "NeuralKey"}),
    };
    let seed_result = ToolCallResult {
        tool_use_id: "tool_s".to_string(),
        content: "hits".to_string(),
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    let mut cache = ToolResultCache::default();
    update_cache(
        &mut cache,
        std::slice::from_ref(&seed),
        std::slice::from_ref(&seed_result),
    );
    assert!(!cache.exact.is_empty());
    assert!(!cache.fuzzy.is_empty());

    // A successful write_file goes through update_cache. The write
    // itself is not cacheable, but the workspace-global slice of the
    // exact cache (`search_code`/`find_files`) and the fuzzy cache
    // must both be cleared. Path-scoped entries (`read_file`,
    // `list_files`, `stat_file`) only invalidate when they overlap
    // the written path — covered by
    // `write_invalidates_only_overlapping_path`.
    let write_call = ToolCallInfo {
        id: "tool_w".to_string(),
        name: "write_file".to_string(),
        input: serde_json::json!({"path": "x.txt", "content": "hello"}),
    };
    let write_result = ToolCallResult {
        tool_use_id: "tool_w".to_string(),
        content: "Wrote 5 bytes to x.txt".to_string(),
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    update_cache(
        &mut cache,
        std::slice::from_ref(&write_call),
        std::slice::from_ref(&write_result),
    );

    assert!(
        cache.exact.is_empty(),
        "search_code exact entries must be cleared by any successful write"
    );
    assert!(
        cache.fuzzy.is_empty(),
        "fuzzy cache must be cleared alongside the exact search_code slice"
    );
}

#[test]
fn failed_write_does_not_clear_caches() {
    // Regression guard for the "any_write" check: only *successful*
    // writes should clear the caches. A failed write_file (e.g. the
    // new chunk guard) must leave the caches intact.
    let seed = ToolCallInfo {
        id: "tool_s".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "NeuralKey"}),
    };
    let seed_result = ToolCallResult {
        tool_use_id: "tool_s".to_string(),
        content: "hits".to_string(),
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    let mut cache = ToolResultCache::default();
    update_cache(
        &mut cache,
        std::slice::from_ref(&seed),
        std::slice::from_ref(&seed_result),
    );

    let failed_write = ToolCallInfo {
        id: "tool_w".to_string(),
        name: "write_file".to_string(),
        input: serde_json::json!({"path": "big.rs", "content": "x"}),
    };
    let failed_result = ToolCallResult {
        tool_use_id: "tool_w".to_string(),
        content: "[CHUNK_GUARD] oversized".to_string(),
        is_error: true,
        kind: aura_core_types::ToolResultKind::AgentError,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    update_cache(
        &mut cache,
        std::slice::from_ref(&failed_write),
        std::slice::from_ref(&failed_result),
    );

    assert!(
        !cache.exact.is_empty(),
        "failed write must NOT clear the exact cache"
    );
    assert!(
        !cache.fuzzy.is_empty(),
        "failed write must NOT clear the fuzzy cache"
    );
}

#[test]
fn split_cached_prefers_exact_over_fuzzy_when_both_match() {
    // Regression guard for the "exact-match stays primary" rule.
    let call = ToolCallInfo {
        id: "tool_1".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "NeuralKey"}),
    };
    let mut cache = ToolResultCache::default();
    cache.exact.insert(
        tool_result_cache_key(&call.name, &call.input),
        "exact-hit".to_string(),
    );
    cache.fuzzy.insert(
        normalized_search_key(&call.name, &call.input).unwrap(),
        "fuzzy-hit".to_string(),
    );

    let (cached, uncached) = split_cached(&[call], &cache);
    assert!(uncached.is_empty());
    assert_eq!(cached.len(), 1);
    // The cached content may be summarized, but it should be derived
    // from the exact hit, not the fuzzy one.
    assert!(
        cached[0].content.contains("exact-hit")
            || cached[0].content.contains("Cached result reused"),
        "expected exact-hit to be preferred, got: {}",
        cached[0].content
    );
    assert!(
        !cached[0].content.contains("fuzzy-hit"),
        "fuzzy value should not leak when exact hit exists"
    );
}

#[test]
fn truncate_preview_uses_ascii_marker() {
    let preview = truncate_preview("abcdef", 3);
    assert_eq!(preview, "abc...");
    assert!(!preview.contains('\u{2026}'));
}

// ---------------------------------------------------------------------------
// Phase 1: range-aware `read_file` cache + path-scoped invalidation
// ---------------------------------------------------------------------------

/// Build the line-numbered `read_file` rendering the way
/// `aura_tools::fs_tools::read::fs_read` emits it (`{:>6}|{content}`,
/// newline-joined). Each line body is the literal `body` string.
fn fake_read_file_rendered(start: usize, end: usize, body: &str) -> String {
    (start..=end)
        .map(|n| format!("{n:>6}|{body}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Drive a single `read_file` call through the cache surface and
/// record whether the underlying executor would have been invoked.
/// Returns the cached `ToolCallResult` (synthesising one with
/// `responder` on miss).
fn drive_read_file(
    cache: &mut ToolResultCache,
    id: &str,
    path: &str,
    start: Option<usize>,
    end: Option<usize>,
    fs_read_calls: &mut usize,
    responder: impl FnOnce(Option<usize>, Option<usize>) -> String,
) -> ToolCallResult {
    let mut input = serde_json::json!({"path": path});
    if let Some(s) = start {
        input["start_line"] = serde_json::json!(s);
    }
    if let Some(e) = end {
        input["end_line"] = serde_json::json!(e);
    }
    let call = ToolCallInfo {
        id: id.to_string(),
        name: "read_file".to_string(),
        input,
    };

    let (cached, uncached) = split_cached(std::slice::from_ref(&call), cache);
    if let Some(hit) = cached.into_iter().next() {
        assert!(
            uncached.is_empty(),
            "miss + hit on the same call is invalid"
        );
        return hit;
    }

    assert_eq!(uncached.len(), 1, "miss must produce one uncached entry");
    *fs_read_calls += 1;
    let content = responder(start, end);
    let result = ToolCallResult {
        tool_use_id: call.id.clone(),
        content,
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    update_cache(
        cache,
        std::slice::from_ref(&call),
        std::slice::from_ref(&result),
    );
    result
}

#[test]
fn read_file_range_cache_serves_subset_from_superset() {
    let mut cache = ToolResultCache::default();
    let mut fs_read_calls = 0usize;
    let path = "src/lib.rs";

    let r_full = drive_read_file(
        &mut cache,
        "tool_1",
        path,
        Some(1),
        Some(150),
        &mut fs_read_calls,
        |start, end| fake_read_file_rendered(start.unwrap(), end.unwrap(), "alpha"),
    );

    let r_subset1 = drive_read_file(
        &mut cache,
        "tool_2",
        path,
        Some(1),
        Some(99),
        &mut fs_read_calls,
        |_, _| panic!("subset 1..99 must hit the range cache without re-executing fs_read"),
    );

    let r_subset2 = drive_read_file(
        &mut cache,
        "tool_3",
        path,
        Some(30),
        Some(100),
        &mut fs_read_calls,
        |_, _| panic!("subset 30..100 must hit the range cache without re-executing fs_read"),
    );

    assert_eq!(
        fs_read_calls, 1,
        "only the superset read should trigger the underlying tool"
    );

    let canonical = "src/lib.rs";
    let entries = cache
        .read_file_by_path
        .get(canonical)
        .expect("per-path index must have a vec for the superset path");
    assert_eq!(
        entries.len(),
        1,
        "subset hits must NOT mint new entries; superset is the only one"
    );

    // The superset's rendered body is the single source of truth for
    // every subset response; checking both subsets came from the same
    // entry (`fs_read_calls == 1` above) plus the line-number prefix
    // assertions below covers the contract end-to-end without
    // requiring the entry to carry a redundant `content_hash`.
    let body1 = r_subset1.content;
    let body2 = r_subset2.content;
    assert!(body1.contains("     1|alpha"));
    assert!(body1.contains("    99|alpha"));
    assert!(!body1.contains("   100|alpha"));
    assert!(body2.contains("    30|alpha"));
    assert!(body2.contains("   100|alpha"));
    assert!(!body2.contains("    29|alpha"));
    // The full superset response must contain every line both subsets
    // claim — confirms both subsets really did slice from the same
    // cached bytes.
    for line in 1..=100 {
        let needle = format!("{:>6}|alpha", line);
        assert!(
            r_full.content.contains(&needle),
            "superset must include line {line}"
        );
    }
}

#[test]
fn write_invalidates_only_overlapping_path() {
    let mut cache = ToolResultCache::default();
    let mut fs_read_calls = 0usize;

    // Cache reads of two unrelated paths.
    drive_read_file(
        &mut cache,
        "read_a",
        "crates/A/foo.rs",
        Some(1),
        Some(50),
        &mut fs_read_calls,
        |s, e| fake_read_file_rendered(s.unwrap(), e.unwrap(), "A"),
    );
    drive_read_file(
        &mut cache,
        "read_b",
        "crates/B/bar.rs",
        Some(1),
        Some(50),
        &mut fs_read_calls,
        |s, e| fake_read_file_rendered(s.unwrap(), e.unwrap(), "B"),
    );
    assert_eq!(fs_read_calls, 2);
    assert_eq!(cache.read_file_by_path.len(), 2);
    assert_eq!(cache.exact.len(), 2);

    // Successful write to crates/A/foo.rs only.
    let write_call = ToolCallInfo {
        id: "tool_w".to_string(),
        name: "write_file".to_string(),
        input: serde_json::json!({"path": "crates/A/foo.rs", "content": "x"}),
    };
    let write_result = ToolCallResult {
        tool_use_id: "tool_w".to_string(),
        content: "wrote".to_string(),
        is_error: false,
        kind: aura_core_types::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
        image: None,
    };
    update_cache(
        &mut cache,
        std::slice::from_ref(&write_call),
        std::slice::from_ref(&write_result),
    );

    assert!(
        !cache.read_file_by_path.contains_key("crates/A/foo.rs"),
        "overlapping read entry must be dropped"
    );
    assert!(
        cache.read_file_by_path.contains_key("crates/B/bar.rs"),
        "non-overlapping read entry must be retained"
    );

    // Subsequent read on crates/B/bar.rs is a HIT (no new fs_read).
    drive_read_file(
        &mut cache,
        "read_b2",
        "crates/B/bar.rs",
        Some(1),
        Some(50),
        &mut fs_read_calls,
        |_, _| panic!("crates/B/bar.rs read should hit the surviving cache entry"),
    );

    // Subsequent read on crates/A/foo.rs is a MISS (fresh fs_read).
    drive_read_file(
        &mut cache,
        "read_a2",
        "crates/A/foo.rs",
        Some(1),
        Some(50),
        &mut fs_read_calls,
        |s, e| fake_read_file_rendered(s.unwrap(), e.unwrap(), "A2"),
    );

    assert_eq!(
        fs_read_calls, 3,
        "only the invalidated path should have triggered a re-read"
    );
}
