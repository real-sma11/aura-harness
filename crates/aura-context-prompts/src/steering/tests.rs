//! Per-kind envelope tests for [`super::SteeringRenderer`] +
//! workspace-wide guardrail scan.
//!
//! - The first batch asserts that the rendered body for a given
//!   [`super::SteeringKind`] is wrapped in the canonical
//!   `<harness_steering kind="…">…</harness_steering>` envelope, and
//!   that the inner wording is preserved verbatim from the pre-PR-D
//!   inline call sites in `task_executor`. These act as the wording
//!   lock — a future change that rewords a steering body has to
//!   update the test in lockstep.
//! - The trailing [`harness_steering_literal_only_in_aura_prompts`]
//!   walks the workspace `crates/` tree at test time and asserts the
//!   literal `<harness_steering` only appears under
//!   `crates/aura-prompts/`. This is the Phase 2 successor to the
//!   `aura-agent::prompts::guardrail_tests` invariant — re-anchored
//!   to the new boundary so the envelope contract still cannot
//!   regress.

use std::fs;
use std::path::{Path, PathBuf};

use super::{SteeringKind, SteeringRenderer, StubReportView};

fn assert_envelope(rendered: &str, label: &str) {
    let expected_open = format!("<harness_steering kind=\"{label}\">\n");
    let expected_close = "\n</harness_steering>";
    assert!(
        rendered.starts_with(&expected_open),
        "expected envelope to open with {expected_open:?}, got:\n{rendered}"
    );
    assert!(
        rendered.ends_with(expected_close),
        "expected envelope to close with {expected_close:?}, got:\n{rendered}"
    );
}

#[test]
fn render_task_done_no_writes_wraps_body_and_preserves_wording() {
    let rendered = SteeringRenderer::render(&SteeringKind::TaskDoneNoWrites);
    assert_envelope(&rendered, "task_done_rejected");
    assert!(
        rendered.contains("ERROR: task_done was rejected — you have not produced any file changes"),
        "no-writes wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("\"no_changes_needed\": true"),
        "escape-hatch wording drifted:\n{rendered}"
    );
}

#[test]
fn render_stub_detected_uses_existing_build_stub_fix_prompt_wording() {
    let reports = vec![StubReportView {
        path: "src/lib.rs".into(),
        line: 42,
        pattern: "todo!() macro".into(),
        context: "fn foo() { todo!() }".into(),
    }];
    let rendered = SteeringRenderer::render(&SteeringKind::StubDetected { reports });
    assert_envelope(&rendered, "stub_detected");
    assert!(
        rendered.contains("STOP: Your implementation compiles but contains stub"),
        "stub-fix preamble drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("src/lib.rs:42"),
        "stub report formatting drifted:\n{rendered}"
    );
}

#[test]
fn render_implement_now_wraps_body_and_preserves_wording() {
    let rendered = SteeringRenderer::render(&SteeringKind::ImplementNow {
        exploration_count: 12,
        sample_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
    });
    assert_envelope(&rendered, "implement_now");
    assert!(
        rendered.contains("12 exploration tools"),
        "exploration count wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("src/a.rs, src/b.rs"),
        "sample paths wording drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("write_file` or `edit_file"),
        "write directive drifted:\n{rendered}"
    );
    assert!(
        rendered.contains("\"no_changes_needed\": true"),
        "escape-hatch wording drifted:\n{rendered}"
    );
}

#[test]
fn render_repeated_read_wraps_body_and_surfaces_short_hash() {
    let rendered = SteeringRenderer::render(&SteeringKind::RepeatedRead {
        content_hash: "deadbeefcafef00d".into(),
    });
    assert_envelope(&rendered, "repeated_read");
    assert!(
        rendered.contains("content_hash=deadbeef"),
        "repeated-read body should surface the leading 8 hex chars of the hash:\n{rendered}"
    );
    assert!(
        rendered.contains("3 times this turn"),
        "repeated-read body should name the firing threshold:\n{rendered}"
    );
    assert!(
        rendered.contains("`start_line`/`end_line`"),
        "repeated-read body should suggest the narrow-range alternative:\n{rendered}"
    );
}

#[test]
fn render_task_already_satisfied_hint_wraps_body_and_carries_command() {
    let rendered = SteeringRenderer::render(&SteeringKind::TaskAlreadySatisfiedHint {
        test_command: "cargo --version".into(),
    });
    assert_envelope(&rendered, "task_already_satisfied");
    assert!(
        rendered.contains("test_command: \"cargo --version\""),
        "rendered body must carry the test_command verbatim:\n{rendered}"
    );
    assert!(
        rendered.contains("test-augmentation mode"),
        "hint body must steer the model toward test-augmentation when the gate already passes:\n{rendered}"
    );
    assert!(
        rendered.contains("harness has not run this command"),
        "minimum-viable variant must explicitly say the test was NOT executed by the harness:\n{rendered}"
    );
}

// ---------------------------------------------------------------------------
// Workspace guardrail: the `<harness_steering` literal only appears
// inside aura-prompts source files.
// ---------------------------------------------------------------------------

const FORBIDDEN_ENVELOPE_LITERAL: &str = "<harness_steering";

#[test]
fn harness_steering_literal_only_in_aura_prompts() {
    // Walk up from CARGO_MANIFEST_DIR (`crates/aura-prompts`) to the
    // workspace root, then enumerate sibling crates.
    let prompts_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = prompts_root
        .parent()
        .expect("aura-prompts must live under crates/")
        .to_path_buf();
    let mut offenders: Vec<String> = Vec::new();
    for entry in fs::read_dir(&crates_dir).expect("workspace crates/ readable") {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path == prompts_root {
            // Legitimate definitions live here.
            continue;
        }
        let src = path.join("src");
        if !src.is_dir() {
            continue;
        }
        walk_rs_files(&src, &mut |file| scan_file(file, &mut offenders));
    }
    assert!(
        offenders.is_empty(),
        "found `<harness_steering` literals outside crates/aura-prompts/ — \
         every steering envelope must be produced by SteeringRenderer. \
         Offenders:\n{}",
        offenders.join("\n"),
    );
}

fn walk_rs_files(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_rs_files(&path, visit);
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
        if trimmed.starts_with("//") {
            continue;
        }
        if !line.contains(FORBIDDEN_ENVELOPE_LITERAL) {
            continue;
        }
        offenders.push(format!("  {}:{}: {}", path.display(), idx + 1, line.trim()));
    }
}
