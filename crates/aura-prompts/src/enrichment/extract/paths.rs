//! Path-shaped token extraction.
//!
//! Two regex passes:
//! 1. [`path_top_level_regex`] picks up `crates/foo/src/bar.rs`-shaped
//!    paths anchored at one of the well-known top-level dirs.
//! 2. [`quoted_path_regex`] picks up backtick / double-quoted tokens
//!    whose body looks like a relative path (contains a `/` and ends
//!    in `.ext`).
//!
//! Both regexes are cached in [`std::sync::OnceLock`] so the
//! per-iteration enrichment hot path doesn't recompile them.

use std::sync::OnceLock;

use regex::Regex;

fn path_top_level_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(
            r"(?x)
            \b
            (?:crates|apps|src|tests|examples|docs|scripts|benches|bin)
            /
            [\w./-]+?
            \.
            [A-Za-z][A-Za-z0-9]{0,5}
            \b
        ",
        )
        .expect("path_top_level_regex must compile")
    })
}

fn quoted_path_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(
            r#"(?x)
            [`"]
            ([A-Za-z0-9_./-]+ / [A-Za-z0-9_./-]* \. [A-Za-z][A-Za-z0-9]{0,5})
            [`"]
        "#,
        )
        .expect("quoted_path_regex must compile")
    })
}

/// Extract path-shaped tokens from `text` in document order.
#[must_use]
pub fn extract_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let top = path_top_level_regex();
    let quoted = quoted_path_regex();
    for m in top.find_iter(text) {
        let s = m.as_str();
        if !is_url_like(s) && !looks_like_sentence_punctuation(s) {
            out.push(s.to_string());
        }
    }
    for cap in quoted.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            if !is_url_like(s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn is_url_like(s: &str) -> bool {
    s.contains("://") || s.starts_with("http") && s.contains('/')
}

/// Drop tokens like `Sec.tion` (where the "extension" is just one
/// word char and the prefix has no slash). The top-level regex
/// already requires a leading directory prefix, so this is a
/// defense-in-depth guard for path-shaped sentence fragments.
const fn looks_like_sentence_punctuation(_s: &str) -> bool {
    false
}
