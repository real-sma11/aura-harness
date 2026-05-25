use aura_reasoner::ContentBlock;
use std::collections::HashMap;

use crate::constants::tool_result_cache_key;
use crate::types::ToolCallInfo;
use crate::types::ToolCallResult;

use super::search_cache::normalized_search_key;
use super::tool_execution::{
    push_tool_result_message_with_context, split_cached, truncate_preview, update_cache,
};

#[test]
fn tool_results_are_emitted_before_context_texts() {
    let mut messages = Vec::new();
    let results = vec![
        ToolCallResult {
            tool_use_id: "tool_1".to_string(),
            content: "ok 1".to_string(),
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
        },
        ToolCallResult {
            tool_use_id: "tool_2".to_string(),
            content: "ok 2".to_string(),
            is_error: true,
            kind: aura_core::ToolResultKind::AgentError,
            stop_loop: false,
            file_changes: Vec::new(),
        },
    ];
    let context = vec!["Build check failed".to_string()];

    push_tool_result_message_with_context(&mut messages, results, context);

    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, aura_reasoner::Role::User);
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
    let mut cache = HashMap::new();
    let long_content = "a".repeat(9_000);
    cache.insert(
        tool_result_cache_key(&call.name, &call.input),
        long_content.clone(),
    );

    let fuzzy_cache = HashMap::new();
    let (cached, uncached) = split_cached(&[call], &cache, &fuzzy_cache);

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
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    };
    let mut cache = HashMap::new();
    let mut fuzzy_cache = HashMap::new();
    update_cache(
        &mut cache,
        &mut fuzzy_cache,
        std::slice::from_ref(&seed),
        std::slice::from_ref(&seed_result),
    );
    assert!(!cache.is_empty(), "exact cache should be populated");
    assert!(!fuzzy_cache.is_empty(), "fuzzy cache should be populated");

    // Now a later call with the alternation terms in a different order
    // — it should MISS the exact cache but HIT the fuzzy cache.
    let reordered = ToolCallInfo {
        id: "tool_query".to_string(),
        name: "search_code".to_string(),
        input: serde_json::json!({"pattern": "NeuralKey|pub fn generate"}),
    };

    assert!(
        !cache.contains_key(&tool_result_cache_key(&reordered.name, &reordered.input)),
        "exact key should not match the reordered alternation"
    );

    let (cached, uncached) = split_cached(&[reordered], &cache, &fuzzy_cache);
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
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    };
    let mut cache = HashMap::new();
    let mut fuzzy_cache = HashMap::new();
    update_cache(
        &mut cache,
        &mut fuzzy_cache,
        std::slice::from_ref(&seed),
        std::slice::from_ref(&seed_result),
    );
    assert!(!cache.is_empty());
    assert!(!fuzzy_cache.is_empty());

    // A successful write_file goes through update_cache. The write
    // itself is not cacheable, but the `any_write` path must clear
    // BOTH the exact and fuzzy caches.
    let write_call = ToolCallInfo {
        id: "tool_w".to_string(),
        name: "write_file".to_string(),
        input: serde_json::json!({"path": "x.txt", "content": "hello"}),
    };
    let write_result = ToolCallResult {
        tool_use_id: "tool_w".to_string(),
        content: "Wrote 5 bytes to x.txt".to_string(),
        is_error: false,
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    };
    update_cache(
        &mut cache,
        &mut fuzzy_cache,
        std::slice::from_ref(&write_call),
        std::slice::from_ref(&write_result),
    );

    assert!(
        cache.is_empty(),
        "exact cache must be cleared by successful write"
    );
    assert!(
        fuzzy_cache.is_empty(),
        "fuzzy cache must be cleared alongside the exact cache"
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
        kind: aura_core::ToolResultKind::Ok,
        stop_loop: false,
        file_changes: Vec::new(),
    };
    let mut cache = HashMap::new();
    let mut fuzzy_cache = HashMap::new();
    update_cache(
        &mut cache,
        &mut fuzzy_cache,
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
        kind: aura_core::ToolResultKind::AgentError,
        stop_loop: false,
        file_changes: Vec::new(),
    };
    update_cache(
        &mut cache,
        &mut fuzzy_cache,
        std::slice::from_ref(&failed_write),
        std::slice::from_ref(&failed_result),
    );

    assert!(
        !cache.is_empty(),
        "failed write must NOT clear the exact cache"
    );
    assert!(
        !fuzzy_cache.is_empty(),
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
    let mut cache = HashMap::new();
    cache.insert(
        tool_result_cache_key(&call.name, &call.input),
        "exact-hit".to_string(),
    );
    let mut fuzzy_cache = HashMap::new();
    fuzzy_cache.insert(
        normalized_search_key(&call.name, &call.input).unwrap(),
        "fuzzy-hit".to_string(),
    );

    let (cached, uncached) = split_cached(&[call], &cache, &fuzzy_cache);
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

