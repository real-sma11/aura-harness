//! Workspace-level boundary test for `aura-prompts`.
//!
//! Phase 2 of the core-loop refactor carved every model-facing string
//! (system prompts, bootstrap context, fix prompts, steering
//! envelopes, compaction-summary auxiliary prompts, and the small set
//! of `model_messages` constants the agent loop and task executor
//! splice into tool-result bodies) out of `aura-agent` and into a
//! dedicated `aura-prompts` crate. The crate is render-only: it
//! depends on `aura-config` plus `serde`/`serde_json`/`regex`/
//! `tracing`/`thiserror` and **must not** depend on `aura-agent`,
//! `aura-automaton`, `aura-runtime`, or `aura-reasoner`.
//!
//! `Cargo.toml` is the primary enforcement mechanism — adding any of
//! the forbidden crates as a dep would fail to compile. This test is
//! the source-level belt-and-suspenders: it scans
//! `crates/aura-prompts/src/**/*.rs` for `use aura_agent::|use
//! aura_automaton::|use aura_runtime::|use aura_reasoner::` and
//! fails CI when one slips in (e.g. through a copy-paste from a
//! migrated module that still carried its old `use` line).

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_USE_PATTERNS: &[&str] = &[
    "use aura_agent::",
    "use aura_automaton::",
    "use aura_runtime::",
    "use aura_reasoner::",
    "use aura_kernel::",
    "use aura_tools::",
];

#[test]
fn aura_prompts_has_no_forbidden_upstream_imports() {
    let prompts_root = workspace_root()
        .join("crates")
        .join("aura-prompts")
        .join("src");
    let mut offenders: Vec<String> = Vec::new();
    visit_rust_sources(&prompts_root, &mut |path, contents| {
        for (idx, line) in contents.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for pat in FORBIDDEN_USE_PATTERNS {
                if line.contains(pat) {
                    offenders.push(format!(
                        "  {}:{}: {} (matched `{}`)",
                        path.display(),
                        idx + 1,
                        line.trim(),
                        pat,
                    ));
                }
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "aura-prompts must stay render-only — forbidden upstream `use` lines found:\n{}\nCargo.toml already forbids these dependencies; if the test is firing, an import slipped in via a copy-paste.",
        offenders.join("\n"),
    );
}

#[test]
fn aura_prompts_cargo_toml_does_not_list_forbidden_deps() {
    let manifest = workspace_root()
        .join("crates")
        .join("aura-prompts")
        .join("Cargo.toml");
    let contents = fs::read_to_string(&manifest)
        .unwrap_or_else(|err| panic!("aura-prompts Cargo.toml unreadable: {err}"));

    // Strip comments so the human-readable note "Forbidden deps: …"
    // doesn't trip the test.
    let stripped: String = contents
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in [
        "aura-agent",
        "aura-automaton",
        "aura-runtime",
        "aura-reasoner",
        "aura-kernel",
        "aura-tools",
    ] {
        assert!(
            !stripped.contains(forbidden),
            "aura-prompts Cargo.toml lists forbidden dep `{forbidden}` (post-comment-strip body):\n{stripped}",
        );
    }
}

/// Phase 2.7 hard-deletes `crates/aura-agent/src/prompts/` (no
/// re-export shim). This test fires the moment the directory reappears
/// so a future contributor cannot quietly re-add prompts code back
/// into `aura-agent`.
#[test]
fn old_prompts_module_is_deleted() {
    let legacy = workspace_root()
        .join("crates")
        .join("aura-agent")
        .join("src")
        .join("prompts");
    assert!(
        !legacy.exists(),
        "Phase 2.7 deleted `crates/aura-agent/src/prompts/`; \
         it must not be recreated (re-export shim or otherwise). \
         Found at: {}",
        legacy.display(),
    );
}

/// Every former `crate::prompts::*` import must now reach the new
/// boundary crate through `aura_prompts::*`. The scan is restricted
/// to `crates/aura-agent/src/**/*.rs` so it does not double-count
/// references inside `aura-prompts` (where the symbol is local).
#[test]
fn aura_agent_does_not_reach_into_old_prompts_path() {
    let src_dir = workspace_root()
        .join("crates")
        .join("aura-agent")
        .join("src");
    let mut offenders: Vec<String> = Vec::new();
    visit_rust_sources(&src_dir, &mut |path, contents| {
        for (idx, raw_line) in contents.lines().enumerate() {
            let trimmed = raw_line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("//!") {
                continue;
            }
            if trimmed.contains("crate::prompts::")
                || trimmed.starts_with("use crate::prompts;")
                || trimmed.starts_with("use crate::prompts::")
            {
                offenders.push(format!(
                    "{}:{}: references the deleted `crate::prompts::*` path — \
                     migrate to `aura_prompts::*`",
                    path.display(),
                    idx + 1,
                ));
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "aura-agent source still references the deleted in-crate prompts module:\n  {}",
        offenders.join("\n  "),
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn visit_rust_sources(root: &Path, visitor: &mut dyn FnMut(&Path, &str)) {
    if !root.exists() {
        return;
    }
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            visit_rust_sources(&path, visitor);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if let Ok(contents) = fs::read_to_string(&path) {
                visitor(&path, &contents);
            }
        }
    }
}
