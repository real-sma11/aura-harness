//! Heuristic guard that flags newly-created Rust source files which
//! aren't reachable from any sibling `mod.rs` / `lib.rs` / `main.rs`.
//!
//! Run after every `write_file` / `edit_file` whose path lives under a
//! Rust `src/` tree. The check is intentionally cheap — it scans only
//! the immediate siblings of the new file, not the full crate — and
//! returns a single boolean flag plus a remediation hint. The caller
//! stamps both into the `ToolResult` metadata so the next-turn prompt
//! can surface the warning without re-running the scan.
//!
//! Why this exists: a previous task created `crates/zero-crypto/src/
//! hpke_hybrid.rs` but never added `pub mod hpke_hybrid;` to
//! `lib.rs`. The compiler quietly never compiled the file and the
//! agent submitted `task_done` against a broken workspace. Surfacing
//! the warning at write-time gives the agent a chance to fix the link
//! before its next tool call.

use std::path::{Path, PathBuf};

use crate::sandbox::Sandbox;

/// Result of the module-link check. Both fields are `pub(crate)` so the
/// caller can stamp them onto `ToolResult` metadata.
#[derive(Debug, Clone)]
pub(crate) struct ModuleLinkCheck {
    /// True when the new file has at least one sibling that declares
    /// `mod <stem>` (with or without `pub`).
    pub linked: bool,
    /// Human-readable message describing the missing link, populated
    /// only when `linked == false`. Includes the candidate parent
    /// modules that were inspected so the agent has somewhere
    /// concrete to add the declaration.
    pub message: Option<String>,
}

/// Check whether the file at `resolved_path` (already written by the
/// caller) has a `mod` declaration in a sibling parent module. Returns
/// `None` when the file isn't a Rust source file under a `src/` tree
/// or when it's itself a module-root (`mod.rs`, `lib.rs`, `main.rs`),
/// for which the linkage check doesn't apply.
pub(crate) fn check_module_link(
    sandbox: &Sandbox,
    relative_path: &str,
    resolved_path: &Path,
) -> Option<ModuleLinkCheck> {
    let stem = rust_module_stem(resolved_path)?;
    if !under_rust_src_tree(relative_path) {
        return None;
    }
    let parent = resolved_path.parent()?;
    let candidates = parent_module_candidates(parent);
    let inspected: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|p| p.starts_with(sandbox.root()))
        .filter(|p| p.is_file())
        .collect();

    if inspected.is_empty() {
        return None;
    }

    let linked = inspected.iter().any(|candidate| {
        std::fs::read_to_string(candidate)
            .map(|text| contains_mod_declaration(&text, &stem))
            .unwrap_or(false)
    });

    if linked {
        Some(ModuleLinkCheck {
            linked: true,
            message: None,
        })
    } else {
        let candidate_list: Vec<String> = inspected
            .iter()
            .filter_map(|p| {
                p.strip_prefix(sandbox.root())
                    .ok()
                    .and_then(|stripped| stripped.to_str())
                    .map(|s| s.replace('\\', "/"))
            })
            .collect();
        let candidate_blob = if candidate_list.is_empty() {
            "<sibling module file>".to_string()
        } else {
            candidate_list.join(" or ")
        };
        Some(ModuleLinkCheck {
            linked: false,
            message: Some(format!(
                "Created '{relative_path}' but no sibling module declares `mod {stem};` — \
                 cargo will silently ignore the file. Add `pub mod {stem};` (or `mod {stem};`) \
                 to {candidate_blob} before relying on the new module."
            )),
        })
    }
}

/// Extract the module stem (`hpke_hybrid` from `hpke_hybrid.rs`) when
/// `path` is a regular Rust source file. Returns `None` for module-root
/// files because those don't need a `mod` declaration in a sibling.
fn rust_module_stem(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    if !file_name.ends_with(".rs") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    if matches!(stem, "mod" | "lib" | "main" | "build") {
        return None;
    }
    Some(stem.to_string())
}

/// True when the **relative** path lives inside a `src/` tree somewhere
/// (e.g. `crates/foo/src/bar/baz.rs` or `src/lib/mod_a.rs`). We deliberately
/// inspect the relative path (rather than the resolved absolute path) so
/// the check fires even when the sandbox itself happens to be rooted under
/// a directory called `src` for unrelated reasons.
fn under_rust_src_tree(relative_path: &str) -> bool {
    let normalised: String = relative_path.replace('\\', "/");
    normalised.split('/').any(|segment| segment == "src")
}

/// Build the list of sibling files that would conventionally declare
/// the new module: a sibling `mod.rs` (for `dir/foo.rs`), and the
/// crate roots `lib.rs` / `main.rs` if the file is directly under
/// `src/`.
fn parent_module_candidates(parent: &Path) -> Vec<PathBuf> {
    let mut out = Vec::with_capacity(3);
    out.push(parent.join("mod.rs"));
    if parent.file_name().and_then(|n| n.to_str()) == Some("src") {
        out.push(parent.join("lib.rs"));
        out.push(parent.join("main.rs"));
    }
    out
}

/// True when `text` contains a `mod <stem>` or `pub mod <stem>`
/// declaration (or `pub(crate) mod`, `pub(super) mod`, etc.). The
/// `;` / `{` terminator is required so we don't accidentally match
/// `mod foo_bar;` when looking for `mod foo`.
fn contains_mod_declaration(text: &str, stem: &str) -> bool {
    for raw_line in text.lines() {
        let line = strip_line_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }
        let after_visibility = strip_visibility_prefix(line);
        let Some(rest) = after_visibility.strip_prefix("mod ") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(name_end) = rest.find(|c: char| !is_ident_char(c)) else {
            // `mod foo` with no terminator (rare but harmless).
            if rest == stem {
                return true;
            }
            continue;
        };
        let name = &rest[..name_end];
        if name == stem {
            let terminator = rest[name_end..].trim_start().chars().next();
            if matches!(terminator, Some(';') | Some('{')) {
                return true;
            }
        }
    }
    false
}

fn strip_line_comment(line: &str) -> &str {
    if let Some(idx) = line.find("//") {
        &line[..idx]
    } else {
        line
    }
}

fn strip_visibility_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("pub(") {
        if let Some(end) = rest.find(')') {
            return rest[end + 1..].trim_start();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("pub ") {
        return rest.trim_start();
    }
    trimmed
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_plain_mod_declaration() {
        assert!(contains_mod_declaration("mod hpke_hybrid;", "hpke_hybrid"));
        assert!(contains_mod_declaration(
            "pub mod hpke_hybrid;",
            "hpke_hybrid"
        ));
        assert!(contains_mod_declaration(
            "pub(crate) mod hpke_hybrid;",
            "hpke_hybrid"
        ));
        assert!(contains_mod_declaration(
            "pub(super) mod hpke_hybrid {",
            "hpke_hybrid"
        ));
    }

    #[test]
    fn does_not_confuse_similar_names() {
        assert!(!contains_mod_declaration(
            "mod hpke_hybrid_inner;",
            "hpke_hybrid"
        ));
        assert!(!contains_mod_declaration(
            "// mod hpke_hybrid;",
            "hpke_hybrid"
        ));
        assert!(!contains_mod_declaration(
            "use crate::hpke_hybrid;",
            "hpke_hybrid"
        ));
    }

    #[test]
    fn rust_module_stem_filters_module_roots() {
        assert_eq!(
            rust_module_stem(Path::new("crates/foo/src/bar.rs")).as_deref(),
            Some("bar")
        );
        assert_eq!(rust_module_stem(Path::new("src/lib.rs")), None);
        assert_eq!(rust_module_stem(Path::new("src/mod.rs")), None);
        assert_eq!(rust_module_stem(Path::new("src/main.rs")), None);
        assert_eq!(rust_module_stem(Path::new("notes.md")), None);
    }

    #[test]
    fn detects_src_tree() {
        assert!(under_rust_src_tree("crates/foo/src/bar.rs"));
        assert!(under_rust_src_tree("src/baz.rs"));
        assert!(under_rust_src_tree("crates\\foo\\src\\bar.rs"));
        assert!(!under_rust_src_tree("crates/foo/tests/bar.rs"));
        assert!(!under_rust_src_tree("notes/src_notes.md"));
    }
}
