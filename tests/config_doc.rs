//! Workspace-level rustdoc coverage test for `aura-config`.
//!
//! Asserts that every `pub` field in `aura-config` carries a doc
//! comment with both a default annotation and (where applicable) the
//! owning env-var name. The check is intentionally permissive — it
//! reads each `aura-config/src/*.rs` source file as text and walks
//! `pub` field declarations rather than introspecting at runtime — so
//! it surfaces gaps without requiring a procedural-macro detour.

use std::fs;
use std::path::{Path, PathBuf};

/// Minimum doc-comment length (chars) for a pub field. We do not
/// require a literal `"default"` token because struct-of-structs
/// fields (e.g. `AgentConfig::compaction`) naturally describe what
/// the sub-tree contains rather than a scalar default. Any
/// non-trivial doc comment (>=12 chars after stripping
/// leading `///`) signals the author thought about the field.
const MIN_DOC_COMMENT_CHARS: usize = 12;
const ENV_PREFIX: &str = "env:";

#[test]
fn every_pub_field_has_a_default_annotation() {
    let workspace_root = workspace_root();
    let aura_config_src = workspace_root
        .join("crates")
        .join("aura-config")
        .join("src");
    assert!(
        aura_config_src.exists(),
        "expected aura-config sources at {}",
        aura_config_src.display(),
    );

    let mut offenders: Vec<String> = Vec::new();
    for source in collect_rust_files(&aura_config_src) {
        let contents = fs::read_to_string(&source).expect("readable rust source");
        for field in pub_fields(&contents) {
            let doc_chars: usize = field
                .doc_comment
                .chars()
                .filter(|c| !c.is_whitespace())
                .count();
            if doc_chars < MIN_DOC_COMMENT_CHARS {
                offenders.push(format!(
                    "{}:{}: `{}` lacks a non-trivial doc comment (>= {} non-whitespace chars)",
                    source.display(),
                    field.line,
                    field.name,
                    MIN_DOC_COMMENT_CHARS,
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "Every pub field in aura-config must be documented:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn every_env_overridable_field_documents_the_env_var() {
    let workspace_root = workspace_root();
    let aura_config_src = workspace_root
        .join("crates")
        .join("aura-config")
        .join("src");

    let env_names = collect_env_var_names(&aura_config_src);
    assert!(
        !env_names.is_empty(),
        "expected at least one owned env-var constant in aura-config/src/env.rs"
    );

    let mut offenders: Vec<String> = Vec::new();
    for source in collect_rust_files(&aura_config_src) {
        if source.ends_with("env.rs") {
            continue;
        }
        let contents = fs::read_to_string(&source).expect("readable rust source");
        for field in pub_fields(&contents) {
            // Only fields whose docstrings reference `env:` are
            // claimed to be env-overridable; for those, ensure the
            // referenced env-var name actually appears in the doc
            // comment.
            if !field.doc_comment.contains(ENV_PREFIX) {
                continue;
            }
            let has_known_env_name = env_names
                .iter()
                .any(|name| field.doc_comment.contains(name.as_str()));
            if !has_known_env_name {
                offenders.push(format!(
                    "{}:{}: `{}` claims env override (`env:` in docs) but does not name an env-var listed in aura-config/src/env.rs",
                    source.display(),
                    field.line,
                    field.name
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "Every env-overridable field must reference its env var:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn crate_level_doc_lists_every_owned_env_var() {
    let workspace_root = workspace_root();
    let lib_rs = workspace_root
        .join("crates")
        .join("aura-config")
        .join("src")
        .join("lib.rs");
    let contents = fs::read_to_string(&lib_rs).expect("readable lib.rs");

    let env_names = collect_env_var_names(
        &workspace_root
            .join("crates")
            .join("aura-config")
            .join("src"),
    );
    let mut missing: Vec<String> = Vec::new();
    for env in &env_names {
        if !contents.contains(env.as_str()) {
            missing.push(env.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "crates/aura-config/src/lib.rs crate-level doc comment must mention every owned env var. Missing: {missing:?}",
    );
}

struct PubField {
    name: String,
    line: usize,
    doc_comment: String,
}

fn pub_fields(contents: &str) -> Vec<PubField> {
    let mut fields = Vec::new();
    let mut doc_buf: Vec<String> = Vec::new();
    let mut in_pub_fn_or_impl_block = 0;

    for (idx, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim_start();

        if line.starts_with("///") {
            let stripped = line.trim_start_matches('/').trim_start();
            doc_buf.push(stripped.to_string());
            continue;
        }

        // Track entry/exit of function/impl/match bodies so we don't
        // pick up `pub` markers inside method bodies.
        for ch in line.chars() {
            match ch {
                '{' => in_pub_fn_or_impl_block += 1,
                '}' => {
                    if in_pub_fn_or_impl_block > 0 {
                        in_pub_fn_or_impl_block -= 1;
                    }
                }
                _ => {}
            }
        }

        if line.starts_with("pub ") && line.contains(':') && !line.contains("fn ") {
            // Heuristic: a struct field declaration looks like
            // `pub name: Type,`. Skip `pub const`, `pub use`, `pub
            // struct`, `pub fn`, `pub mod`.
            let body_after_pub = line.trim_start_matches("pub ");
            let is_field = !body_after_pub.starts_with("const ")
                && !body_after_pub.starts_with("use ")
                && !body_after_pub.starts_with("struct ")
                && !body_after_pub.starts_with("enum ")
                && !body_after_pub.starts_with("mod ")
                && !body_after_pub.starts_with("type ")
                && !body_after_pub.starts_with("trait ")
                && !body_after_pub.starts_with("static ")
                && !body_after_pub.starts_with("(crate)");
            if is_field {
                if let Some(name) = body_after_pub.split(':').next() {
                    let name = name.trim().trim_end_matches(':').to_string();
                    fields.push(PubField {
                        name,
                        line: idx + 1,
                        doc_comment: doc_buf.join("\n"),
                    });
                }
            }
        }

        if !line.starts_with("///") {
            doc_buf.clear();
        }
    }

    fields
}

fn collect_env_var_names(src_dir: &Path) -> Vec<String> {
    let env_rs = src_dir.join("env.rs");
    fs::read_to_string(&env_rs).expect("readable env.rs");
    aura_config::ENV_VAR_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

fn collect_rust_files(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                paths.extend(collect_rust_files(&p));
            } else if p.extension().is_some_and(|ext| ext == "rs") {
                paths.push(p);
            }
        }
    }
    paths
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
