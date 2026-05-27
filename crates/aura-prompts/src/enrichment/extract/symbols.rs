//! Symbol-shaped token extraction.
//!
//! Four regex passes (deduped through a `HashSet` at the call site):
//! - [`rust_path_camel_regex`] — `Foo::bar`-shaped paths whose
//!   leading segment is CamelCase.
//! - [`rust_path_snake_regex`] — `crate::item`-shaped paths whose
//!   leading segment is snake_case.
//! - [`backtick_ident_regex`] — backtick-wrapped identifiers.
//! - [`bare_camel_regex`] — bare CamelCase identifiers in prose.
//!
//! All regexes are cached in [`std::sync::OnceLock`].

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

fn rust_path_camel_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"\b[A-Z][A-Za-z0-9_]+::[A-Za-z0-9_]+\b")
            .expect("rust_path_camel_regex must compile")
    })
}

fn rust_path_snake_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"\b[a-z][a-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)+\b")
            .expect("rust_path_snake_regex must compile")
    })
}

fn backtick_ident_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"`([A-Za-z_][A-Za-z0-9_]*)`").expect("backtick_ident_regex must compile")
    })
}

fn bare_camel_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"\b(?:[A-Z][a-z]+(?:[A-Z][a-z]*|[A-Z]+)+|[A-Z][a-z]{2,}|[A-Z]{3,6})\b")
            .expect("bare_camel_regex must compile")
    })
}

/// English stopwords and Rust-keyword-shaped tokens that the
/// snake-case backtick regex picks up but that have ~zero chance of
/// being a real workspace symbol. Kept small and high-signal; the
/// cost of a few false-positive grep calls is much lower than the
/// cost of dropping a real symbol.
const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "this",
    "that",
    "from",
    "into",
    "into_",
    "self",
    "true",
    "false",
    "none",
    "some",
    "ok",
    "err",
    "fn",
    "pub",
    "impl",
    "struct",
    "trait",
    "enum",
    "type",
    "mod",
    "use",
    "let",
    "mut",
    "ref",
    "where",
    "match",
    "if",
    "else",
    "loop",
    "while",
    "for_each",
    "return",
    "break",
    "continue",
    "as",
    "in",
    "of",
    "on",
    "to",
    "at",
    "by",
    "or",
    "not",
    "is",
    "be",
    "do",
    "we",
    "you",
    "it",
    "an",
    "a",
    "todo",
    "fixme",
    "note",
    "tip",
    "warning",
    "see",
    "test",
    "tests",
    // bare_camel_regex (Phase A) coverage:
    "refactor",
    "implement",
    "implementation",
    "then",
    "defines",
    "define",
];

/// Extract symbol-shaped tokens from `text` in document order.
#[must_use]
pub fn extract_symbols(text: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for m in rust_path_camel_regex().find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for m in rust_path_snake_regex().find_iter(text) {
        let s = m.as_str().to_string();
        if seen.insert(s.clone()) {
            out.push(s);
        }
    }
    for cap in backtick_ident_regex().captures_iter(text) {
        if let Some(m) = cap.get(1) {
            let s = m.as_str();
            if is_plausible_ident(s) {
                let owned = s.to_string();
                if seen.insert(owned.clone()) {
                    out.push(owned);
                }
            }
        }
    }
    for m in bare_camel_regex().find_iter(text) {
        let s = m.as_str();
        if is_plausible_camel_ident(s) {
            let owned = s.to_string();
            if seen.insert(owned.clone()) {
                out.push(owned);
            }
        }
    }

    out
}

fn is_plausible_ident(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if STOPWORDS.contains(&lower.as_str()) {
        return false;
    }
    // Need at least one uppercase letter OR an underscore — bare
    // lowercase words like `enqueue` would also match, but they're
    // common English-ish nouns and produce mostly noise without
    // context.
    s.chars().any(|c| c.is_ascii_uppercase()) || s.contains('_')
}

fn is_plausible_camel_ident(s: &str) -> bool {
    if s.len() < 3 {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    !STOPWORDS.contains(&lower.as_str())
}
