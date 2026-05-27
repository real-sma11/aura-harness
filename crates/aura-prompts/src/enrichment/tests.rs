//! Tests for the pure half of `enrichment` (extraction + render).
//!
//! IO-side tests (FsWorkspace, resolve_hints) live in
//! `aura-agent/src/prompt_resolve/tests.rs` because that's where the
//! `WorkspaceReader` trait + `resolve_hints` orchestration are
//! implemented (per the Phase 2 boundary contract).

use super::extract::extract_hints;
use super::extract::symbols::extract_symbols;
use super::render::into_block;
use super::types::{ContextHints, ResolvedContext, ResolvedPath, ResolvedSymbol, SymbolHit};

#[test]
fn extract_hints_picks_paths_and_symbols() {
    let desc = "Wire Publisher::enqueue in crates/zero-network/src/publisher.rs \
                to spawn_driver in crates/zero-storage/src/outbox.rs. The \
                `Outbox` type already exists; reuse its `enqueue_batch` helper.";
    let hints = extract_hints(desc);
    assert!(
        hints
            .paths
            .contains(&"crates/zero-network/src/publisher.rs".to_string()),
        "expected publisher.rs in paths, got {:?}",
        hints.paths
    );
    assert!(
        hints
            .paths
            .contains(&"crates/zero-storage/src/outbox.rs".to_string()),
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
        "expected enqueue_batch in symbols, got {:?}",
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
        assert!(!p.starts_with("http"), "URL leaked into hints: {p}");
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
fn extract_hints_cf_task_surfaces_sibling_reference() {
    let hints = extract_hints("2.6 outbox CF");
    assert!(
        hints.module_keywords.iter().any(|m| m == "outbox"),
        "expected outbox module keyword in {hints:?}"
    );
    assert!(
        hints.module_keywords.iter().any(|m| m == "inbox"),
        "expected inbox sibling keyword in {hints:?}"
    );
    assert!(
        hints.module_keywords.iter().any(|m| m == "storage"),
        "expected storage anchor in {hints:?}"
    );
    assert!(
        hints.symbols.iter().any(|s| s == "OutboxEntry"),
        "expected OutboxEntry symbol hint in {hints:?}"
    );
    assert!(
        hints
            .module_note
            .as_deref()
            .is_some_and(|note| note.contains("outbox.rs")),
        "expected missing-module note in {hints:?}"
    );
}

#[test]
fn extracts_snake_case_crate_path() {
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
    let desc = "We use `Outbox` for queueing. The Outbox type \
                backs zero_storage::Outbox in the storage crate.";
    let symbols = extract_symbols(desc);
    let outbox_count = symbols.iter().filter(|s| s.as_str() == "Outbox").count();
    assert_eq!(
        outbox_count, 1,
        "expected exactly one bare Outbox, got {symbols:?}",
    );
    assert!(
        symbols.iter().any(|s| s == "zero_storage::Outbox"),
        "expected zero_storage::Outbox alongside bare Outbox in {symbols:?}",
    );
}

#[test]
fn render_empty_resolved_context_returns_empty_string() {
    let resolved = ResolvedContext::default();
    assert_eq!(into_block(&resolved), "");
}

#[test]
fn render_paths_and_symbols_into_block() {
    let resolved = ResolvedContext {
        paths: vec![ResolvedPath {
            path: "crates/zero-storage/src/outbox.rs".into(),
            head: Some("pub struct Outbox;\n".into()),
            head_line_count: 1,
        }],
        symbols: vec![ResolvedSymbol {
            symbol: "Outbox::enqueue".into(),
            hits: vec![SymbolHit {
                path: "crates/zero-storage/src/outbox.rs".into(),
                line: 84,
                text: "pub fn enqueue(&mut self, item: Item) {".into(),
            }],
        }],
        module_note: None,
        max_block_chars: 4000,
    };
    let block = into_block(&resolved);
    assert!(block.contains("## Pre-resolved context"));
    assert!(block.contains("crates/zero-storage/src/outbox.rs"));
    assert!(block.contains("pub struct Outbox"));
    assert!(block.contains("Outbox::enqueue"));
    assert!(block.contains("outbox.rs:84"));
    assert!(block.contains("starting points"));
}

#[test]
fn render_drops_bodies_when_over_budget() {
    let big_body = "fn line() {}\n".repeat(200);
    let resolved = ResolvedContext {
        paths: vec![
            ResolvedPath {
                path: "crates/a/src/lib.rs".into(),
                head: Some(big_body.clone()),
                head_line_count: 200,
            },
            ResolvedPath {
                path: "crates/b/src/lib.rs".into(),
                head: Some(big_body),
                head_line_count: 200,
            },
        ],
        symbols: vec![],
        module_note: None,
        max_block_chars: 400,
    };
    let block = into_block(&resolved);
    assert!(block.contains("crates/a/src/lib.rs"));
    assert!(block.contains("crates/b/src/lib.rs"));
    assert!(
        block.len() <= 1000,
        "expected block trimmed near budget, got {} chars",
        block.len()
    );
}

#[test]
fn meaningful_returns_true_when_any_field_populated() {
    let mut hints = ContextHints::default();
    assert!(!hints.is_meaningful());
    hints.paths.push("crates/foo/src/lib.rs".into());
    assert!(hints.is_meaningful());
}
