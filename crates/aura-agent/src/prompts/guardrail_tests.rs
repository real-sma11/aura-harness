//! Guardrail tests enforcing PR D's "model-facing strings live under
//! `crates/aura-agent/src/prompts/`" invariant.
//!
//! These compile-time-walked filesystem assertions scan the crate
//! source tree at test time and reject inline string literals that
//! contain forbidden model-facing tokens outside the
//! [`crate::prompts`] module. The signal is intentionally narrow
//! (the literal substring `<harness_steering`) so the test does not
//! emit false positives on every legitimate `format!` block — the
//! point is to lock the envelope contract: any new
//! `<harness_steering …>` literal that appears outside
//! [`crate::prompts::steering`] is a regression and fails CI loudly.
//!
//! Choosing the envelope marker as the canary keeps the test simple
//! (no syntax-aware parser, no exception list bookkeeping) while
//! still catching the regression PR D is designed to prevent:
//! someone bypassing the [`crate::prompts::steering::SteeringInjector`]
//! by hand-writing an envelope inline. Stub fix prompts, task_done
//! gate rejections, and `apply_patch` diagnostics now all route
//! through the injector; this test ensures they stay routed.
//!
//! The walker scans `.rs` files under `crates/aura-agent/src/` and
//! skips the `prompts/` subtree entirely so the legitimate envelope
//! definitions in [`crate::prompts::steering::injector`] are not
//! flagged.

use std::fs;
use std::path::{Path, PathBuf};

/// Phrase that flags a model-facing envelope leak when found
/// outside the `prompts/` subtree.
const FORBIDDEN_ENVELOPE_LITERAL: &str = "<harness_steering";

#[test]
fn no_harness_steering_literal_outside_prompts_module() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_root = crate_root.join("src");
    let prompts_root = src_root.join("prompts");

    let mut offenders: Vec<String> = Vec::new();
    walk_rs_files(&src_root, &prompts_root, &mut |path| {
        scan_file(path, &mut offenders);
    });

    assert!(
        offenders.is_empty(),
        "found `<harness_steering` literals outside crates/aura-agent/src/prompts/ — \
         every steering envelope must be produced by SteeringInjector. \
         Offenders:\n{}",
        offenders.join("\n"),
    );
}

fn walk_rs_files(dir: &Path, prompts_root: &Path, visit: &mut impl FnMut(&Path)) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.starts_with(prompts_root) {
                continue;
            }
            walk_rs_files(&path, prompts_root, visit);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            visit(&path);
        }
    }
}

fn scan_file(path: &Path, offenders: &mut Vec<String>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    for (idx, line) in contents.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("//!") {
            continue;
        }
        if !line.contains(FORBIDDEN_ENVELOPE_LITERAL) {
            continue;
        }
        offenders.push(format!(
            "  {}:{}: {}",
            path.display(),
            idx + 1,
            line.trim()
        ));
    }
}
