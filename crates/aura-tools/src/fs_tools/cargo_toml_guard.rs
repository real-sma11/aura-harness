//! Structural guard for `Cargo.toml` writes.
//!
//! Runs after every `write_file` / `edit_file` that lands on a path
//! ending in `Cargo.toml`. Rejects writes that:
//!
//! 1. Fail to parse as TOML at all (`toml::from_str::<toml::Value>`
//!    returns an error). This catches busted brackets, unterminated
//!    strings, accidental indentation, etc., before the agent moves on
//!    and discovers the breakage via `cargo check`.
//! 2. Declare the same dependency key more than once under any of the
//!    canonical `[dependencies]`, `[dev-dependencies]`,
//!    `[build-dependencies]`, or `[target.*.dependencies]` tables. The
//!    parser strictly errors on duplicate keys inside the same table,
//!    but the resulting message ("duplicate key `foo`") doesn't tell
//!    the agent *which* table or where; this guard surfaces both.
//!
//! The guard is invoked from [`crate::fs_tools::write::fs_write`] and
//! [`crate::fs_tools::edit::fs_edit`] after they've already finalised
//! the on-disk content. A failure rolls back nothing — the tool returns
//! `ToolError::InvalidArguments` describing the structural problem and
//! the caller (the agent loop) can decide to retry with corrected
//! content. We deliberately keep the post-hoc check rather than a
//! pre-flight check so the existing file content (used for diff
//! computation, shrinkage guards, etc.) doesn't have to be re-read.

use std::collections::HashMap;
use std::path::Path;

use crate::error::ToolError;

/// Names of dependency tables we structurally guard. Anything under
/// `target.<cfg>.dependencies` etc. matches via prefix; the bare names
/// below are matched exactly.
const DEPENDENCY_TABLE_NAMES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// Does this path look like a Cargo manifest? Matches the file name
/// case-insensitively so Windows paths with mixed case still trip the
/// guard.
#[must_use]
pub(crate) fn is_cargo_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Cargo.toml"))
}

/// Run the structural guard on `content`. Returns `Ok(())` for healthy
/// manifests and `Err(ToolError::InvalidArguments(msg))` when the
/// content would leave the file in a broken state.
pub(crate) fn validate_cargo_toml(content: &str) -> Result<(), ToolError> {
    if let Err(parse_err) = toml::from_str::<toml::Value>(content) {
        return Err(ToolError::InvalidArguments(format!(
            "Cargo.toml does not parse as TOML: {parse_err}. \
             Fix the structural error before retrying — the file would otherwise \
             break `cargo` for every subsequent command in this workspace."
        )));
    }
    if let Some(report) = find_duplicate_dependency_keys(content) {
        return Err(ToolError::InvalidArguments(format!(
            "Cargo.toml declares the same dependency key more than once: {report}. \
             Remove the duplicate entries; cargo refuses to compile a manifest with \
             repeated keys."
        )));
    }
    Ok(())
}

/// Walk the manifest line-by-line and return a diagnostic listing any
/// dependency key that appears more than once inside the same
/// dependency table.
///
/// Implemented as a manual scan (rather than reading off
/// `toml::Value`) because the parser already rejects duplicates with a
/// generic "duplicate key" message — by the time we reached this code
/// path the parser accepted the file, but we want to ALSO catch the
/// pathological case where the parser implementation is lenient OR the
/// duplicates straddle table re-opens (e.g. a feature gate re-opens
/// `[dependencies]` lower in the file and accidentally repeats a
/// crate name).
fn find_duplicate_dependency_keys(content: &str) -> Option<String> {
    let mut current_table: Option<String> = None;
    // Per-table set of keys we've seen so far. Keys leaked into the
    // diagnostic list as we go.
    let mut seen: HashMap<String, Vec<String>> = HashMap::new();
    let mut duplicates: Vec<(String, String, usize)> = Vec::new();

    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim_start();
        let line_no = idx + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = parse_table_header(line) {
            current_table = Some(header);
            continue;
        }
        let Some(table) = current_table.as_ref() else {
            continue;
        };
        if !is_dependency_table(table) {
            continue;
        }
        let Some(key) = parse_key_assignment(line) else {
            continue;
        };
        let entry = seen.entry(table.clone()).or_default();
        if entry.iter().any(|existing| existing == &key) {
            duplicates.push((table.clone(), key, line_no));
        } else {
            entry.push(key);
        }
    }

    if duplicates.is_empty() {
        None
    } else {
        let formatted: Vec<String> = duplicates
            .into_iter()
            .map(|(table, key, line)| format!("`{key}` in [{table}] (line {line})"))
            .collect();
        Some(formatted.join(", "))
    }
}

/// Extract the canonical table name from a `[...]` or `[[...]]` header
/// line. Returns the inner text trimmed of whitespace; returns `None`
/// for non-header lines.
fn parse_table_header(line: &str) -> Option<String> {
    if line.starts_with("[[") {
        let end = line.find("]]")?;
        return Some(line[2..end].trim().to_string());
    }
    if line.starts_with('[') {
        let end = line.find(']')?;
        return Some(line[1..end].trim().to_string());
    }
    None
}

/// True for `dependencies`, `dev-dependencies`, `build-dependencies`,
/// and any `target.<cfg>.dependencies` / `target.<cfg>.dev-dependencies`
/// / `target.<cfg>.build-dependencies` variant.
fn is_dependency_table(table: &str) -> bool {
    if DEPENDENCY_TABLE_NAMES.contains(&table) {
        return true;
    }
    if let Some(rest) = table.strip_prefix("target.") {
        let suffix = rest.rsplit('.').next().unwrap_or("");
        return DEPENDENCY_TABLE_NAMES.contains(&suffix);
    }
    false
}

/// Pull the bare key out of `<key> = ...`. Handles bare keys, quoted
/// keys, and ignores trailing comments. Returns `None` when the line
/// doesn't look like an assignment.
fn parse_key_assignment(line: &str) -> Option<String> {
    let eq = line.find('=')?;
    let raw_key = line[..eq].trim();
    if raw_key.is_empty() {
        return None;
    }
    // Dotted keys (foo.bar = ...) — only the first segment is the
    // crate name we care about for duplicate detection. Cargo accepts
    // dotted keys under [dependencies] for inline-table style, e.g.
    // `serde.version = "1"`. We split first, then strip quotes from
    // the leading segment so `"serde".version = "1"` still collapses
    // to `serde`.
    let first_segment = raw_key.split('.').next()?.trim();
    if first_segment.is_empty() {
        return None;
    }
    Some(strip_surrounding_quotes(first_segment).to_string())
}

fn strip_surrounding_quotes(key: &str) -> &str {
    if key.len() >= 2
        && ((key.starts_with('"') && key.ends_with('"'))
            || (key.starts_with('\'') && key.ends_with('\'')))
    {
        &key[1..key.len() - 1]
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_clean_manifest() {
        let manifest = r#"
[package]
name = "foo"
version = "0.1.0"

[dependencies]
serde = "1"
tokio = { version = "1", features = ["full"] }

[dev-dependencies]
tempfile = "3"
"#;
        assert!(validate_cargo_toml(manifest).is_ok());
    }

    #[test]
    fn rejects_unparseable_toml() {
        let broken = "[dependencies\nserde = \"1\"\n";
        let err = validate_cargo_toml(broken).unwrap_err();
        let ToolError::InvalidArguments(msg) = err else {
            panic!("expected InvalidArguments");
        };
        assert!(msg.contains("does not parse as TOML"));
    }

    #[test]
    fn detects_duplicate_dependency_via_manual_scan() {
        // Two `[dependencies]` blocks both declaring `serde` — the
        // strict TOML parser would catch this, but we exercise the
        // manual scan path here too so its diagnostics are covered.
        let manifest = "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\
                        [dependencies]\nserde = \"1\"\n";
        // Inject a synthetic second table by appending; we check the
        // scanner directly because the toml parser would reject the
        // composite via its own duplicate-key error first.
        let scanned =
            find_duplicate_dependency_keys(&format!("{manifest}\n[dependencies]\nserde = \"2\"\n"));
        let report = scanned.expect("scanner must report duplicate `serde`");
        assert!(report.contains("`serde`"), "report: {report}");
        assert!(report.contains("[dependencies]"), "report: {report}");
    }

    #[test]
    fn dotted_key_collapses_to_first_segment() {
        assert_eq!(
            parse_key_assignment("serde.version = \"1\"").as_deref(),
            Some("serde")
        );
        assert_eq!(
            parse_key_assignment("\"serde\".version = \"1\"").as_deref(),
            Some("serde")
        );
    }

    #[test]
    fn recognises_target_specific_dependency_tables() {
        assert!(is_dependency_table("dependencies"));
        assert!(is_dependency_table("dev-dependencies"));
        assert!(is_dependency_table("build-dependencies"));
        assert!(is_dependency_table("target.cfg(unix).dependencies"));
        assert!(is_dependency_table(
            "target.\"cfg(target_os = \\\"linux\\\")\".dev-dependencies"
        ));
        assert!(!is_dependency_table("package"));
        assert!(!is_dependency_table("features"));
    }

    #[test]
    fn is_cargo_manifest_is_case_insensitive() {
        assert!(is_cargo_manifest(Path::new("Cargo.toml")));
        assert!(is_cargo_manifest(Path::new("crates/foo/Cargo.toml")));
        assert!(is_cargo_manifest(Path::new("CARGO.TOML")));
        assert!(!is_cargo_manifest(Path::new("Cargo.lock")));
        assert!(!is_cargo_manifest(Path::new("notes.toml")));
    }
}
