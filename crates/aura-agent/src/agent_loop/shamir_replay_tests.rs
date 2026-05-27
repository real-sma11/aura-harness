//! Phase 6 — Shamir-recovery benchmark replay harness.
//!
//! This test module drives the captured 1.7 Shamir-recovery
//! `read_file` sequence (checked in under
//! `crates/aura-agent/tests/fixtures/shamir_recovery_transcript.json`)
//! through three of the reread-efficiency plan's already-shipped
//! pieces:
//!
//! 1. Phase 1's range-aware [`ToolResultCache`] in
//!    [`super::tool_execution`] — every subset/superset hit avoids a
//!    re-execution.
//! 2. Phase 2's
//!    [`aura_compaction::dedup_read_results_by_content_hash`] pass —
//!    identical read-only tool results fold to a structured marker
//!    in older history.
//! 3. Phase 3's [`crate::prompts::steering::RepeatedReadTracker`] —
//!    three identical `content_hash` observations queue a steering
//!    nudge for the next turn.
//!
//! The fixture is a JSON array of `{tool_name, input, result_bytes,
//! content_hash}` records (the latter two are checked-in stubs); the
//! actual `result_bytes` is synthesised from `(path, start_line,
//! end_line)` plus the leading metadata record's `_files` line-count
//! map so the fixture stays compact. Synthesis is deterministic so
//! every replay run sees the same byte payload and `content_hash`.
//!
//! ## Deferred — oracle short-circuit
//!
//! The plan's third floor — `task_already_satisfied` short-circuiting
//! before any `edit_file` — is **not** asserted here. Phase 3
//! ([`crate::prompts::steering::EarlyTestOracle`] and
//! [`crate::prompts::steering::RepeatedReadTracker`]) is unit-tested
//! at the type level, but [`super::LoopState::begin_iteration`] /
//! [`super::tool_execution::handle_tool_use`] do not observe either
//! today. The replay therefore pins only the three floors that are
//! reachable through already-wired runtime code. The end-to-end
//! oracle assertion lands when Phase 3 is integrated into the loop.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use aura_compaction::dedup_read_results_by_content_hash;
use aura_reasoner::{ContentBlock, Message, Role, ToolResultContent};
use serde::Deserialize;
use serde_json::Value;

use crate::prompts::steering::{RepeatedReadTracker, SteeringKind, REPEATED_READ_THRESHOLD};
use crate::types::{ToolCallInfo, ToolCallResult};

use super::tool_execution::{split_cached, update_cache};
use super::ToolResultCache;

/// Maximum distinct `read_file` executions the Phase 1 cache should
/// produce when replaying the fixture. Down from the 30+ executions
/// the original task transcript observed.
const SHAMIR_REPLAY_MAX_DISTINCT_EXECUTIONS: usize = 8;

/// One `read_file` call captured from the Shamir-recovery transcript.
/// `result_bytes` / `content_hash` are checked-in stubs — the test
/// computes the canonical body and hash from `(path, start, end)`
/// plus the per-file line-count map at the top of the fixture.
#[derive(Debug, Deserialize)]
struct TranscriptEntry {
    tool_name: String,
    input: Value,
}

/// Load the fixture, returning `(line_counts, reads)`. The leading
/// JSON object in the array carries metadata under `_doc` / `_files`;
/// every subsequent object is a tool-call record.
fn load_transcript() -> (HashMap<String, usize>, Vec<TranscriptEntry>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("shamir_recovery_transcript.json");
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("read shamir fixture at {}: {e}", path.display());
    });
    let raw_entries: Vec<Value> =
        serde_json::from_str(&raw).expect("fixture must parse as a JSON array");

    let mut line_counts: HashMap<String, usize> = HashMap::new();
    let mut reads: Vec<TranscriptEntry> = Vec::new();

    for value in raw_entries {
        let obj = value.as_object().expect("each fixture entry is an object");
        if obj.contains_key("_files") {
            if let Some(files) = obj.get("_files").and_then(Value::as_object) {
                for (path, count) in files {
                    let n = count
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .expect("file line count must be a non-negative integer");
                    line_counts.insert(path.clone(), n);
                }
            }
            continue;
        }
        let entry: TranscriptEntry = serde_json::from_value(value)
            .expect("non-metadata fixture entries must be tool-call records");
        reads.push(entry);
    }
    assert!(
        !line_counts.is_empty(),
        "fixture metadata `_files` map must seed the line-count table"
    );
    assert!(
        !reads.is_empty(),
        "fixture must contain at least one read_file record"
    );
    (line_counts, reads)
}

/// Per-line body used to synthesise a deterministic file payload.
/// Sized so that a handful of full-file reads exceed the dedup marker
/// envelope (~110 bytes) and the Phase 2 assertion proves real byte
/// savings rather than noise.
fn line_body(path: &str, n: usize) -> String {
    format!(
        "// {path} line {n}: filler payload to make the replay fixture exercise dedup byte math"
    )
}

fn full_file_text(path: &str, line_count: usize) -> String {
    let mut out = String::with_capacity(line_count * 80);
    for n in 1..=line_count {
        out.push_str(&line_body(path, n));
        out.push('\n');
    }
    out
}

fn line_numbered_slice(
    path: &str,
    line_count: usize,
    start: Option<usize>,
    end: Option<usize>,
) -> String {
    let s = start.unwrap_or(1).max(1);
    let e = end.unwrap_or(line_count).min(line_count);
    if e < s {
        return String::new();
    }
    let mut rows: Vec<String> = Vec::with_capacity(e - s + 1);
    for n in s..=e {
        rows.push(format!("{:>6}|{}", n, line_body(path, n)));
    }
    rows.join("\n")
}

/// Derive the canonical rendered `read_file` output for a fixture
/// entry. Unbounded calls return the raw whole-file body (matches
/// what `aura_tools::fs_tools::read::fs_read` emits when neither
/// `start_line` nor `end_line` is supplied); bounded calls return the
/// `{:>6}|{content}` line-numbered slice (matches the same tool's
/// bounded branch).
fn synthesize_result_bytes(
    entry: &TranscriptEntry,
    line_counts: &HashMap<String, usize>,
) -> String {
    let path = entry.input["path"]
        .as_str()
        .expect("read_file input must include a string `path`");
    let line_count = *line_counts
        .get(path)
        .unwrap_or_else(|| panic!("fixture `_files` map missing line count for {path}"));
    let start = entry
        .input
        .get("start_line")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    let end = entry
        .input
        .get("end_line")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok());
    if start.is_none() && end.is_none() {
        full_file_text(path, line_count)
    } else {
        line_numbered_slice(path, line_count, start, end)
    }
}

/// Mirror of `aura_tools::fs_tools::read::content_hash_hex` /
/// `aura_compaction::messages::dedup_content_hash_hex`. Reproduced
/// here so the replay harness has zero new dependencies and the hash
/// stamp matches what the read tool would have produced for the same
/// bytes (both implementations use `std::hash::DefaultHasher`).
fn content_hash_hex(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Phase 1 floor: replay the Shamir transcript through the
/// range-aware [`ToolResultCache`] and assert the distinct-execution
/// count stays under [`SHAMIR_REPLAY_MAX_DISTINCT_EXECUTIONS`].
#[test]
fn shamir_replay_meets_eight_distinct_read_target() {
    let (line_counts, transcript) = load_transcript();
    let mut cache = ToolResultCache::default();
    let mut distinct_executions = 0usize;

    for (idx, entry) in transcript.iter().enumerate() {
        let call = ToolCallInfo {
            id: format!("tu_{idx:04}"),
            name: entry.tool_name.clone(),
            input: entry.input.clone(),
        };
        let (cached, uncached) = split_cached(std::slice::from_ref(&call), &cache);
        if uncached.is_empty() {
            assert_eq!(
                cached.len(),
                1,
                "entry {idx} should resolve to exactly one cached hit"
            );
            continue;
        }
        assert_eq!(
            uncached.len(),
            1,
            "entry {idx} should resolve to exactly one cache miss"
        );
        assert!(
            cached.is_empty(),
            "entry {idx} cannot be both cached and uncached"
        );

        distinct_executions += 1;
        let body = synthesize_result_bytes(entry, &line_counts);
        let result = ToolCallResult {
            tool_use_id: call.id.clone(),
            content: body,
            is_error: false,
            kind: aura_core::ToolResultKind::Ok,
            stop_loop: false,
            file_changes: Vec::new(),
        };
        update_cache(
            &mut cache,
            std::slice::from_ref(&call),
            std::slice::from_ref(&result),
        );
    }

    assert!(
        distinct_executions <= SHAMIR_REPLAY_MAX_DISTINCT_EXECUTIONS,
        "Shamir replay must stay at or below the {SHAMIR_REPLAY_MAX_DISTINCT_EXECUTIONS}-distinct-execution target, got {distinct_executions} (total reads: {})",
        transcript.len(),
    );
    assert!(
        distinct_executions < transcript.len(),
        "the cache must have served at least one hit; replay was a full miss \
         ({distinct_executions} executions / {} reads)",
        transcript.len(),
    );

    // Surface the headline number so developers running the replay
    // (`cargo test -p aura-agent --lib shamir_replay -- --nocapture`)
    // see the cache's actual distinct-execution count next to the
    // floor.
    println!(
        "shamir_replay cache: {distinct_executions} distinct executions across {} reads (floor <= {SHAMIR_REPLAY_MAX_DISTINCT_EXECUTIONS})",
        transcript.len(),
    );
}

/// Phase 2 floor: feed the same transcript through
/// [`dedup_read_results_by_content_hash`] and assert (a) the number
/// of folds matches the count of *redundant* identical-text
/// occurrences and (b) the resulting prompt-bytes footprint shrinks
/// below the no-dedup baseline.
#[test]
fn shamir_replay_compaction_reduces_prompt_bytes() {
    let (line_counts, transcript) = load_transcript();

    let mut messages: Vec<Message> = Vec::with_capacity(transcript.len() * 2);
    let mut baseline_bytes = 0usize;
    let mut occurrences: HashMap<String, usize> = HashMap::new();

    for (idx, entry) in transcript.iter().enumerate() {
        let id = format!("tu_{idx:04}");
        let body = synthesize_result_bytes(entry, &line_counts);
        baseline_bytes += body.len();
        *occurrences.entry(body.clone()).or_insert(0) += 1;

        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                &id,
                &entry.tool_name,
                entry.input.clone(),
            )],
        });
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(
                &id,
                ToolResultContent::Text(body),
                false,
            )],
        });
    }

    // Phase 2 keeps the NEWEST verbatim and folds every older
    // identical copy, so the expected fold count is `n-1` for every
    // text body that appears `n >= 2` times.
    let expected_folds: usize = occurrences.values().map(|c| c.saturating_sub(1)).sum();
    assert!(
        expected_folds > 0,
        "fixture must contain at least one redundant read for the dedup floor to be meaningful"
    );

    let folded = dedup_read_results_by_content_hash(&mut messages);
    assert!(
        folded >= expected_folds,
        "compaction dedup must fold every redundant identical read \
         (expected >= {expected_folds}, got {folded}; distinct bodies = {})",
        occurrences.len(),
    );

    let after_bytes: usize = messages
        .iter()
        .flat_map(|m| &m.content)
        .map(|b| match b {
            ContentBlock::ToolResult {
                content: ToolResultContent::Text(t),
                ..
            } => t.len(),
            _ => 0,
        })
        .sum();
    assert!(
        after_bytes < baseline_bytes,
        "compaction dedup must reduce prompt bytes ({after_bytes} bytes after vs {baseline_bytes} baseline)"
    );

    // Surface the percentage reduction in the test output so the
    // developer running the replay sees it in `cargo test --
    // --nocapture` without needing extra plumbing.
    let saved = baseline_bytes - after_bytes;
    #[allow(clippy::cast_precision_loss)]
    let saved_pct = (saved as f64) * 100.0 / (baseline_bytes as f64);
    println!(
        "shamir_replay compaction: {folded} folds, {saved} bytes saved ({saved_pct:.1}% of {baseline_bytes} baseline)"
    );
}

/// Phase 3 floor: feed the same transcript's `content_hash` sequence
/// through [`RepeatedReadTracker`] and assert that the third
/// identical hash queues a [`SteeringKind::RepeatedRead`] nudge for
/// the next turn.
#[test]
fn shamir_replay_steering_fires_on_repeated_hash() {
    let (line_counts, transcript) = load_transcript();
    let mut tracker = RepeatedReadTracker::new();
    let mut nudges_fired = 0usize;
    let mut firing_hashes: Vec<String> = Vec::new();

    for entry in &transcript {
        let body = synthesize_result_bytes(entry, &line_counts);
        let hash = content_hash_hex(body.as_bytes());
        if tracker.record(&hash) {
            nudges_fired += 1;
            firing_hashes.push(hash);
        }
    }

    assert!(
        nudges_fired >= 1,
        "third identical content_hash must trigger at least one nudge (REPEATED_READ_THRESHOLD = {REPEATED_READ_THRESHOLD})"
    );
    assert_eq!(
        tracker.pending_count(),
        nudges_fired,
        "every threshold-crossing record() call should queue exactly one pending nudge"
    );

    // Draining via `begin_turn` is the contract the agent loop's
    // future Phase 3 wiring will use; this assertion locks in that
    // the queued nudges materialise as `SteeringKind::RepeatedRead`
    // values keyed by the firing `content_hash`.
    let drained = tracker.begin_turn();
    assert_eq!(drained.len(), nudges_fired);
    for kind in &drained {
        match kind {
            SteeringKind::RepeatedRead { content_hash } => {
                assert!(
                    firing_hashes.contains(content_hash),
                    "drained nudge content_hash {content_hash} must come from the firing set"
                );
            }
            other => panic!("expected RepeatedRead, got {other:?}"),
        }
    }
    assert_eq!(
        tracker.pending_count(),
        0,
        "begin_turn must drain every queued nudge"
    );
}
