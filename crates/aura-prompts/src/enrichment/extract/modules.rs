//! Module-keyword extraction + sibling expansion.
//!
//! `expand_module_hints` runs a small lowercase-identifier extractor
//! over the task description, surfaces the lowercase keywords that
//! are also known module families (e.g. `outbox` siblings → `inbox`),
//! and threads a missing-module note when the task talks about a
//! module that does not exist yet (the `outbox` case).
//!
//! The lowercase extractor is intentionally a small subset of
//! `aura-agent::file_ops::task_keywords::extract_task_keywords` —
//! keeping a focused copy here means the prompts crate has no
//! dependency on `aura-agent` (per the Phase 2 boundary contract).

use std::sync::OnceLock;

use regex::Regex;

use super::super::types::ContextHints;
use super::ordered_unique;

fn cf_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| Regex::new(r"(?i)\b(?:cf|column\s*family)\b").expect("cf regex"))
}

fn module_keyword_regex() -> &'static Regex {
    static CELL: OnceLock<Regex> = OnceLock::new();
    CELL.get_or_init(|| {
        Regex::new(r"\b([a-z][a-z0-9_]{2,})\b").expect("module_keyword_regex must compile")
    })
}

/// Lowercase keywords that look like module names but are too
/// common to surface. Subset of
/// `aura-agent::file_ops::task_keywords::COMMON_MODULE_STOP_WORDS`,
/// trimmed to the cases that show up in dev-loop task descriptions.
const COMMON_MODULE_STOP_WORDS: &[&str] = &[
    "the",
    "this",
    "that",
    "with",
    "from",
    "into",
    "each",
    "some",
    "none",
    "and",
    "for",
    "not",
    "are",
    "but",
    "all",
    "any",
    "can",
    "has",
    "was",
    "will",
    "use",
    "its",
    "let",
    "new",
    "our",
    "try",
    "may",
    "should",
    "must",
    "also",
    "just",
    "than",
    "then",
    "when",
    "who",
    "how",
    "what",
    "pub",
    "mod",
    "impl",
    "self",
    "super",
    "crate",
    "where",
    "type",
    "struct",
    "enum",
    "trait",
    "async",
    "await",
    "move",
    "return",
    "true",
    "false",
    "mut",
    "ref",
    "str",
    "run",
    "set",
    "get",
    "add",
    "using",
    "create",
    "implement",
    "update",
    "delete",
    "task",
    "file",
    "code",
    "test",
    "build",
    "make",
    "does",
    "like",
    "have",
    "been",
];

fn extract_lowercase_module_keywords(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for cap in module_keyword_regex().captures_iter(text) {
        let word = cap[1].to_string();
        if !COMMON_MODULE_STOP_WORDS.contains(&word.as_str()) && seen.insert(word.clone()) {
            out.push(word);
        }
    }
    out
}

/// Expand CF / module-task hints from lowercase keywords and sibling
/// maps. See module-level docs.
pub(super) fn expand_module_hints(text: &str, hints: &mut ContextHints) {
    let cf_task = cf_regex().is_match(text);

    let keywords = extract_lowercase_module_keywords(text);
    let mut modules: Vec<String> = keywords
        .into_iter()
        .filter(|k| k.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        .filter(|k| k.len() >= 3)
        .collect();

    let has_known_module = modules
        .iter()
        .any(|kw| !module_siblings(kw.as_str()).is_empty());
    if !cf_task && !has_known_module {
        return;
    }

    for kw in modules.clone() {
        for sibling in module_siblings(kw.as_str()) {
            if !modules.contains(&(*sibling).to_string()) {
                modules.push((*sibling).to_string());
            }
        }
    }

    if modules.iter().any(|m| m == "outbox") {
        if !hints.symbols.iter().any(|s| s.contains("OutboxEntry")) {
            hints.symbols.push("OutboxEntry".to_string());
        }
        hints.module_note = Some(
            "Note: `outbox.rs` / `OutboxEntry` may not exist yet — implement them \
             using the inbox / storage patterns below."
                .to_string(),
        );
    }

    if cf_task {
        for anchor in ["storage", "lib"] {
            if !modules.contains(&(*anchor).to_string()) {
                modules.push((*anchor).to_string());
            }
        }
    }

    hints.module_keywords = ordered_unique(modules);

    if cf_task || !hints.module_keywords.is_empty() {
        hints.symbols.truncate(64);
    }
}

fn module_siblings(name: &str) -> &'static [&'static str] {
    match name {
        "outbox" => &["inbox"],
        "inbox" => &["outbox"],
        _ => &[],
    }
}
