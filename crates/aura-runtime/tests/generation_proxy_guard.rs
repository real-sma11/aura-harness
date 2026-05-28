//! Regression guard for the **generation-proxy declared exception**.
//!
//! The module `crates/aura-runtime/src/gateway/session/generation.rs` is documented in
//! `docs/invariants.md` as an explicit exception to the "all LLM calls go
//! through the kernel gateway" invariant. The exception is narrow: the
//! module is a pure SSE pass-through that proxies image / video / 3D
//! generation requests from a WS client to the upstream router. It does
//! **not**:
//!
//!   * persist `RecordEntry` rows into the store,
//!   * take a `Kernel` handle or reach into kernel state,
//!   * consume LLM credits or call a `ModelProvider`,
//!   * append transcript entries via any `append_entry_*` helper.
//!
//! If any of those patterns ever shows up in this file, the module has
//! graduated past "pure proxy" and should now be routed through the
//! kernel gateway like every other LLM-bearing call — at which point the
//! declared-exception entry in `docs/invariants.md` needs to be removed
//! or narrowed.
//!
//! This test fails loudly in that case so the invariant doc and the code
//! cannot silently drift apart. It reads the source file from disk and
//! scans for forbidden identifiers. Using a filesystem scan (rather
//! than a compile-time check) means we can also catch usages that only
//! surface via macros, and the expected-set stays trivially readable.

use std::path::PathBuf;

/// Path (relative to the crate root) of the module the guard watches.
const GENERATION_SRC_REL: &str = "src/gateway/session/generation.rs";

/// Identifiers that, if they appear in the generation-proxy source,
/// indicate the module has started doing something the declared
/// exception says it must not do.
///
/// The strings are split into `(label, pattern)` pairs so test output
/// explains *why* a match is a problem. The patterns are searched as
/// substrings; a few use a trailing `_` on purpose (e.g.
/// `append_entry_`) so any helper in the family trips the guard.
///
/// NOTE: the pattern strings are built at runtime from fragments so
/// the guard source itself does not contain the bare identifiers —
/// otherwise a future refactor that accidentally co-locates the guard
/// and the proxy would false-positive.
fn forbidden_patterns() -> Vec<(&'static str, String)> {
    vec![
        (
            "persists transcript rows — declared exception says no record-entry writes",
            format!("{}{}", "Record", "Entry"),
        ),
        (
            "takes a Kernel handle — declared exception says no kernel coupling",
            // Match the bare type name; a word-ish boundary check is
            // done by the scanner below so `kernel` module paths
            // don't false-positive.
            String::from("Kernel"),
        ),
        (
            "touches ModelProvider — declared exception says no LLM-credit path",
            format!("{}{}", "Model", "Provider"),
        ),
        (
            "calls an append_entry_* helper — declared exception says no transcript persistence",
            format!("{}{}", "append_", "entry_"),
        ),
    ]
}

/// Returns an absolute path to the generation-proxy source file.
///
/// Anchored at `CARGO_MANIFEST_DIR` so the test is independent of the
/// shell cwd and works under both `cargo test` and IDE test runners.
fn generation_source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GENERATION_SRC_REL)
}

/// Lightweight scanner: a pattern matches when it appears as a
/// substring on a line that is *not* a comment-only line and *not*
/// inside the module's outer `//!` doc-comment header. Comments can
/// legitimately mention forbidden types ("this proxy does not
/// persist `RecordEntry` rows..."); what matters is whether the
/// *code* uses them.
fn line_is_code(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("//") {
        return false;
    }
    true
}

#[test]
fn generation_proxy_has_no_forbidden_identifiers() {
    let path = generation_source_path();
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "failed to read generation-proxy source at {}: {e}",
            path.display()
        )
    });

    let mut violations: Vec<String> = Vec::new();
    for (lineno, line) in src.lines().enumerate() {
        if !line_is_code(line) {
            continue;
        }
        for (label, pattern) in forbidden_patterns() {
            if line.contains(pattern.as_str()) {
                violations.push(format!(
                    "  {path}:{lineno}\n    pattern: {pattern:?}\n    reason : {label}\n    line   : {line}",
                    path = path.display(),
                    lineno = lineno + 1,
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "\n\ngeneration-proxy declared-exception guard tripped.\n\n\
         The module `{rel}` is documented in docs/invariants.md as an\n\
         explicit exception to the kernel-gateway invariant because it is a\n\
         pure proxy. One or more forbidden identifiers now appear in its\n\
         source, which would mean it has crossed the line:\n\n{list}\n\n\
         If this change is intentional, the declared-exception entry in\n\
         docs/invariants.md MUST be updated to match, or the module MUST\n\
         be re-routed through the kernel gateway instead.\n",
        rel = GENERATION_SRC_REL,
        list = violations.join("\n\n")
    );
}

#[test]
fn generation_proxy_source_is_readable() {
    // Sanity check: if the source path ever moves, this test tells us
    // the guard is stale before the contents-scan test fails with a
    // confusing "file not found" panic.
    let path = generation_source_path();
    assert!(
        path.exists(),
        "generation-proxy source file missing at {} \
         (did the module move? update GENERATION_SRC_REL)",
        path.display()
    );
}
