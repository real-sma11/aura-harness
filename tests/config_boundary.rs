//! Workspace-level boundary test for `aura-config`.
//!
//! Phase 1 of the core-loop refactor centralized every owned env-var
//! read and every previously-scattered magic constant into the
//! `aura-config` crate. This test enforces that single-source-of-truth
//! by scanning the entire workspace for two failure modes:
//!
//! 1. Direct `std::env::var(...)` reads of any env-var name owned by
//!    `aura-config` outside the `aura-config` crate itself.
//! 2. Numeric / string literal occurrences of the migrated magic
//!    values outside the `aura-config` crate. These are detected by a
//!    short curated allow-list so the test stays maintainable: only
//!    the literal values that are most distinctive / unlikely to
//!    appear naturally elsewhere are scanned.
//!
//! When a hit is found, the failure message points at the offending
//! file path and explains how to migrate the call to the
//! `aura_config` accessors.

use std::fs;
use std::path::{Path, PathBuf};

const OWNED_ENV_VARS: &[&str] = &[
    "AURA_AGENT_DISABLE_COMPACTION",
    "AURA_AGENT_IMPLEMENT_NOW",
    "AURA_AGENT_IMPLEMENT_NOW_THRESHOLD",
    "AURA_AGENT_IMPLEMENT_NOW_BLOCK",
    "AURA_AGENT_BOOTSTRAP_SPEC_BYTES",
    "AURA_AGENT_BOOTSTRAP_STRIP_CODE_FENCES",
    "AURA_AGENT_BOOTSTRAP_CONTEXT_CHARS",
    "AURA_TURN_TOOL_HEARTBEAT_INTERVAL_SECS",
    "AURA_DOD_TEST_COMMAND",
    "AURA_SIMPLE_MODEL",
    "AURA_LLM_MAX_RETRIES",
    "AURA_LLM_BACKOFF_INITIAL_MS",
    "AURA_LLM_BACKOFF_CAP_MS",
    "AURA_DEV_LOOP_ENABLED_THINKING",
];

/// Per-migrated-const scan: each entry is `(const_name, owning_path)`.
///
/// Detects re-declarations of constants migrated into `aura-config`.
/// We match on the exact `const FOO_BAR:` declaration shape so a
/// numeric literal that legitimately appears elsewhere (e.g. another
/// crate's unrelated `12_000`) does not trip the test.
const MIGRATED_CONST_NAMES: &[&str] = &[
    "CACHEABLE_TOOLS",
    "WRITE_FILE_CHUNK_BYTES",
    "WRITE_FILE_HARD_MAX_BYTES",
    "MAX_ITERATIONS",
    "AUTO_BUILD_COOLDOWN",
    "THINKING_TAPER_AFTER",
    "THINKING_TAPER_FACTOR",
    "THINKING_MIN_BUDGET",
    "THINKING_AUTO_ENABLE_THRESHOLD",
    "BUDGET_WARNING_30",
    "BUDGET_WARNING_40_NO_WRITE",
    "BUDGET_WARNING_60",
    "CHARS_PER_TOKEN",
    "COMPACTION_TIER_HISTORY",
    "COMPACTION_TIER_AGGRESSIVE",
    "COMPACTION_TIER_60",
    "COMPACTION_TIER_30",
    "COMPACTION_TIER_MICRO",
    "MAX_TASK_CONTEXT_CHARS",
    "DEFAULT_BOOTSTRAP_TASK_CONTEXT_CHARS",
    "MAX_WORK_LOG_TASK_CONTEXT",
    "READS_AFTER_WRITE_ALLOWANCE",
    "TOOL_ERROR_PREVIEW_LIMIT",
    "BUILD_FIX_SNAPSHOT_BUDGET",
    "ERROR_SOURCE_BUDGET",
    "GIT_READ_TIMEOUT_SECS",
    "REPEATED_READ_THRESHOLD",
    "REPEATED_READ_HASH_DISPLAY_CHARS",
    "IMPLEMENT_NOW_DEFAULT_THRESHOLD",
    "IMPLEMENT_NOW_MAX_PATHS_IN_MESSAGE",
    "REFINEMENT_MAX_TOKENS",
    "SPEC_GEN_MAX_TOKENS",
    "DEV_LOOP_RETRY_NOTE_MAX_BYTES",
    "PERVASIVE_ERROR_MIN_CALLS",
    "PERVASIVE_ERROR_THRESHOLD",
    "RECENT_OUTCOMES_WINDOW",
    "PROMPT_COMPACTION_MAX_BLOCK_CHARS",
    "BOOTSTRAP_SPEC_DEFAULT_BYTES",
    "MAX_STUB_FIX_ATTEMPTS",
];

/// Allow-list of `(crate, const_name)` pairs where the name shadowing
/// is legitimate (e.g. aura-tools has its own, unrelated
/// `WRITE_FILE_CHUNK_BYTES` constant for its tool-sandbox layer; it
/// intentionally lives outside aura-config because the tool-sandbox
/// boundary is out of scope per Phase 1's plan).
const SHADOW_ALLOWLIST: &[(&str, &str)] = &[
    // `aura-tools` keeps its own per-tool write-cap constant for the
    // executor-side guard. The agent-side cap lives in `aura-config`;
    // the two are intentionally separate.
    ("aura-tools", "WRITE_FILE_CHUNK_BYTES"),
];

#[test]
fn no_crate_outside_aura_config_reads_owned_env_vars_directly() {
    let workspace_root = workspace_root();
    let crates_dir = workspace_root.join("crates");
    let mut offenders: Vec<String> = Vec::new();

    visit_rust_sources(&crates_dir, &mut |path, contents| {
        if is_inside_aura_config(path, &workspace_root) {
            return;
        }
        for env_name in OWNED_ENV_VARS {
            for pattern in env_var_patterns(env_name) {
                if contents.contains(pattern.as_str()) {
                    offenders.push(format!(
                        "{}: contains `{}` — migrate to `aura_config::{}().…` accessor or use `aura_config::install_for_test`",
                        path.display(),
                        pattern,
                        accessor_hint(env_name),
                    ));
                }
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "Crates other than aura-config must not read owned env vars directly:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn no_crate_outside_aura_config_redeclares_migrated_consts() {
    let workspace_root = workspace_root();
    let crates_dir = workspace_root.join("crates");
    let mut offenders: Vec<String> = Vec::new();

    visit_rust_sources(&crates_dir, &mut |path, contents| {
        if is_inside_aura_config(path, &workspace_root) {
            return;
        }
        let crate_name = crate_name_for(path, &workspace_root);
        for name in MIGRATED_CONST_NAMES {
            if SHADOW_ALLOWLIST
                .iter()
                .any(|(c, n)| *c == crate_name && *n == *name)
            {
                continue;
            }
            for line in contents.lines() {
                let trimmed = line.trim_start();
                if !trimmed.contains(&format!("const {name}")) {
                    continue;
                }
                // Allow re-export aliases that simply bind the
                // crate-local name to `aura_config::<NAME>`. These
                // exist so external test fixtures can keep
                // referencing the local name without paying the
                // module-rename cost.
                if line.contains("aura_config::") {
                    continue;
                }
                offenders.push(format!(
                    "{}: declares `const {}` — migrated to `aura_config::{}`",
                    path.display(),
                    name,
                    name,
                ));
            }
        }
    });

    assert!(
        offenders.is_empty(),
        "Crates other than aura-config must not redeclare migrated consts:\n  {}",
        offenders.join("\n  ")
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn is_inside_aura_config(path: &Path, root: &Path) -> bool {
    let config_dir = root.join("crates").join("aura-config");
    path.starts_with(config_dir)
}

/// Best-effort: returns the workspace crate name for a source path
/// inside `crates/<name>/...`. Returns `""` for files outside any
/// crate directory.
fn crate_name_for(path: &Path, workspace_root: &Path) -> String {
    let crates_dir = workspace_root.join("crates");
    let Ok(relative) = path.strip_prefix(&crates_dir) else {
        return String::new();
    };
    relative
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn env_var_patterns(name: &str) -> Vec<String> {
    vec![
        format!("std::env::var(\"{name}\")"),
        format!("env::var(\"{name}\")"),
        format!("env::set_var(\"{name}\""),
        format!("env::remove_var(\"{name}\")"),
        format!("std::env::set_var(\"{name}\""),
        format!("std::env::remove_var(\"{name}\")"),
    ]
}

fn accessor_hint(env_name: &str) -> &'static str {
    if env_name.starts_with("AURA_LLM_") || env_name == "AURA_DEV_LOOP_ENABLED_THINKING" {
        "reasoner"
    } else {
        "agent"
    }
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
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == ".aura-shared-target")
            {
                continue;
            }
            visit_rust_sources(&path, visitor);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            if let Ok(contents) = fs::read_to_string(&path) {
                visitor(&path, &contents);
            }
        }
    }
}
