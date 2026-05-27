//! Secondary, fuzzy cache index for `search_code` / `find_files`.
//!
//! The exact-match cache (`LoopState::tool_cache`, keyed by
//! [`aura_config::tool_result_cache_key`]) stays primary and
//! unchanged. This module provides a second key function that collapses
//! order-of-alternation and trivial whitespace differences, so near-
//! duplicate exploration calls hit the cache instead of re-executing:
//!
//! * `"pub fn generate|NeuralKey"` and `"NeuralKey|pub fn generate"`
//!   produce the same normalized key — alternation terms are trimmed,
//!   lowercased, deduped, and sorted.
//! * Trailing slashes on `path` are stripped so
//!   `"crates/aura-agent/"` and `"crates/aura-agent"` collide.
//!
//! The fuzzy cache is strictly additive: exact hits are preferred (so
//! existing tests and contracts keep passing), and the fuzzy index is
//! cleared alongside the exact cache whenever a successful write
//! happens. See `tool_execution::split_cached` / `update_cache` for the
//! wiring.

use serde_json::Value;

/// Build a normalized secondary cache key for `search_code` /
/// `find_files`. Returns `None` for any other tool so callers can
/// skip non-cacheable work without a separate allow-list.
#[must_use]
pub(crate) fn normalized_search_key(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "search_code" => Some(build_key(
            "search_code",
            &normalize_pattern(input.get("pattern").and_then(Value::as_str)),
            &normalize_scalar(input.get("include").and_then(Value::as_str)),
            &normalize_path(input.get("path").and_then(Value::as_str)),
            &normalize_scalar(input.get("context_lines").map(value_to_display).as_deref()),
        )),
        "find_files" => {
            let pattern = input
                .get("pattern")
                .and_then(Value::as_str)
                .or_else(|| input.get("glob").and_then(Value::as_str));
            Some(build_key(
                "find_files",
                &normalize_pattern(pattern),
                "",
                &normalize_path(input.get("path").and_then(Value::as_str)),
                "",
            ))
        }
        _ => None,
    }
}

fn value_to_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn normalize_scalar(raw: Option<&str>) -> String {
    raw.map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

fn normalize_path(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let unified = trimmed.replace('\\', "/");
    unified.trim_end_matches('/').to_ascii_lowercase()
}

/// Split an alternation string (`a|b|c`) into a canonical, stable form.
///
/// 1. Split on `|`.
/// 2. Trim whitespace from each piece and drop empties.
/// 3. Lowercase.
/// 4. Dedupe (preserving first occurrence) then sort alphabetically.
/// 5. Rejoin with `|`.
///
/// Non-alternated patterns (no `|`) still get trimmed + lowercased so
/// `"NeuralKey"` and `"  neuralkey  "` collide.
fn normalize_pattern(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return String::new();
    };
    let mut pieces: Vec<String> = raw
        .split('|')
        .map(|p| p.trim().to_ascii_lowercase())
        .filter(|p| !p.is_empty())
        .collect();

    // Dedupe while preserving first occurrence, then sort for
    // order-insensitivity.
    let mut seen = std::collections::HashSet::new();
    pieces.retain(|p| seen.insert(p.clone()));
    pieces.sort();
    pieces.join("|")
}

fn build_key(tool: &str, pattern: &str, include: &str, path: &str, ctx: &str) -> String {
    format!("{tool}\0pattern={pattern}\0include={include}\0path={path}\0ctx={ctx}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalized_key_collapses_alternation_variants() {
        let key_a = normalized_search_key(
            "search_code",
            &json!({"pattern": "pub fn generate|NeuralKey"}),
        )
        .unwrap();
        let key_b = normalized_search_key(
            "search_code",
            &json!({"pattern": "NeuralKey|pub fn generate"}),
        )
        .unwrap();

        assert_eq!(
            key_a, key_b,
            "alternation-order reordering must produce identical normalized keys"
        );

        // All three of the spec's variants should share the `neuralkey`
        // term after normalization — verify it's present in each key.
        let key_c = normalized_search_key(
            "search_code",
            &json!({"pattern": "pub fn generate|impl NeuralKey"}),
        )
        .unwrap();
        for key in [&key_a, &key_b, &key_c] {
            assert!(
                key.contains("neuralkey"),
                "every variant should retain the `neuralkey` term, got: {key}"
            );
        }
    }

    #[test]
    fn normalized_key_handles_whitespace_and_case() {
        let a = normalized_search_key("search_code", &json!({"pattern": "  NeuralKey  "})).unwrap();
        let b = normalized_search_key("search_code", &json!({"pattern": "neuralkey"})).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn normalized_key_dedupes_alternation_terms() {
        let a = normalized_search_key("search_code", &json!({"pattern": "Foo|foo|FOO"})).unwrap();
        let b = normalized_search_key("search_code", &json!({"pattern": "foo"})).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn normalized_key_buckets_by_include_path_and_context() {
        let base = normalized_search_key(
            "search_code",
            &json!({"pattern": "NeuralKey", "path": "crates/aura-agent"}),
        )
        .unwrap();
        let trailing_slash = normalized_search_key(
            "search_code",
            &json!({"pattern": "NeuralKey", "path": "crates/aura-agent/"}),
        )
        .unwrap();
        assert_eq!(
            base, trailing_slash,
            "trailing slash on path should be ignored"
        );

        let different_path = normalized_search_key(
            "search_code",
            &json!({"pattern": "NeuralKey", "path": "crates/aura-tools"}),
        )
        .unwrap();
        assert_ne!(base, different_path);

        let different_include = normalized_search_key(
            "search_code",
            &json!({"pattern": "NeuralKey", "path": "crates/aura-agent", "include": "*.rs"}),
        )
        .unwrap();
        assert_ne!(base, different_include);
    }

    #[test]
    fn find_files_accepts_pattern_or_glob() {
        let with_pattern = normalized_search_key(
            "find_files",
            &json!({"pattern": "**/*.rs", "path": "crates/"}),
        )
        .unwrap();
        let with_glob =
            normalized_search_key("find_files", &json!({"glob": "**/*.rs", "path": "crates"}))
                .unwrap();
        assert_eq!(
            with_pattern, with_glob,
            "find_files should accept either `pattern` or `glob` and collapse path trailing-slash"
        );
    }

    #[test]
    fn fuzzy_key_returns_none_for_non_search_tools() {
        assert!(normalized_search_key("read_file", &json!({"path": "src/lib.rs"})).is_none());
        assert!(normalized_search_key("list_files", &json!({"path": "src"})).is_none());
        assert!(normalized_search_key("stat_file", &json!({"path": "src/lib.rs"})).is_none());
        assert!(
            normalized_search_key("write_file", &json!({"path": "x", "content": "y"})).is_none()
        );
        assert!(normalized_search_key("run_command", &json!({"cmd": "ls"})).is_none());
    }
}
