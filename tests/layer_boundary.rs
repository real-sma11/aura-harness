//! Workspace boundary test for the layered crate architecture.
//!
//! Phase 1 deliverable: walks every `crates/*/Cargo.toml`, builds a
//! `crate → layer` map from the new `aura-<layer>-<name>` naming
//! convention (and an explicit `KNOWN_CRATES` table for the legacy
//! pre-refactor crate names), and emits a line for every
//! workspace-internal dependency edge that points "upward" in the
//! layer stack.
//!
//! Phase 6c (context-memory inversion) promotes the
//! `aura-context-memory -> aura-agent` edge from warn-only to
//! fail-on-detect. Every other upward edge that pre-existed is kept
//! warn-only (see [`WARN_ONLY_UPWARD_EDGES`]) until its own phase
//! retires it. Phase 9 will promote the remaining warn-only edges to
//! strict mode.
//!
//! ## Layer order (lowest → highest)
//!
//! `core` < `store` < `config` < `model` < `context` < `plugin` <
//! `exec` < `agent` < `fleet` < `surface`.
//!
//! Equal or downward edges are silently accepted. Upward edges and
//! cross-shortcut edges produce a single `eprintln!` line each.
//!
//! ## Parsing
//!
//! We use a small hand-rolled TOML parser scoped to the `[package]`
//! `name` field and the `[dependencies]`, `[dev-dependencies]`,
//! `[build-dependencies]`, and target-conditional dep tables. Path
//! dependencies referencing `../<other-crate>` are mapped to that
//! crate's name. We deliberately avoid pulling in the `toml` or
//! `cargo_toml` crates as test-only dev deps to keep this test
//! self-contained.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Layer rank — lower numbers depend "downward" (allowed); higher
/// numbers depend "upward" (advisory warning).
const LAYER_ORDER: &[&str] = &[
    "core", "store", "config", "model", "context", "plugin", "exec", "agent", "fleet", "surface",
];

/// Explicit overrides for crate names that don't follow the
/// `aura-<layer>-<name>` convention yet. Empty string means "no
/// layer assigned — skip from the warnings" (most legacy crates fall
/// here today; later phases rename them and remove the entry).
/// Upward dependency edges that have not yet been retired but are
/// accepted for now (warn-only) because they belong to a future phase.
///
/// Each entry is `(consumer, dependency)` matching exactly how the
/// edge appears in the `crates/*/Cargo.toml` files. Adding to this
/// list requires a phase reference and a TODO so it can be retired.
///
/// Phase 6c (context-memory inversion) deliberately omits any
/// `("aura-context-memory", "aura-agent")` entry: that edge is now
/// fail-on-detect. Phase 9 promotes the remaining warn-only edges
/// to strict mode.
const WARN_ONLY_UPWARD_EDGES: &[(&str, &str)] = &[
    // Phase 5 exec layer split left `aura-tools` (a re-export shell
    // that still pulls in the legacy `aura-kernel` crate) as the one
    // unavoidable cross-layer edge. Tracked for cleanup in Phase 9.
    ("aura-tools", "aura-kernel"),
];

const KNOWN_CRATES: &[(&str, &str)] = &[
    // Root binary + dev/CI stuff — not a crate-layer thing.
    ("aura", "surface"),
    ("aura-node", "surface"),
    // Pre-Phase-1 legacy names. The mapping below reflects the
    // intended final layer for each. The layer-rank check uses these
    // assignments, so today's warnings show where the as-is graph
    // does not match the intended layering.
    ("aura-core", "core"),
    ("aura-config", "config"),
    // Phase 2 store layer split:
    //   aura-store        — compatibility shell over aura-store-db
    //   aura-store-db     — RocksDB-backed durable storage impl
    //   aura-store-record — append-only record-log domain types + RecordLog trait
    //   aura-store-snapshot — content-addressed snapshot store (V1 no-op stub)
    ("aura-store", "store"),
    ("aura-store-db", "store"),
    ("aura-store-record", "store"),
    ("aura-store-snapshot", "store"),
    ("aura-tools", "exec"),
    // Phase 5 exec layer split (1 legacy shell + 6 new crates):
    //   aura-exec-conflict  — domain-scoped advisory locks (no aura-* deps)
    //   aura-exec-isolation — worktree + copy isolation (no aura-* deps)
    //   aura-exec-policy    — verdict over EffectivePermissions (depends on aura-core-permissions)
    //   aura-exec-sandbox   — FsSandbox + ProcessSandbox primitives (no aura-* deps)
    //   aura-exec-tools     — re-export shell over aura-tools + sandbox + policy
    //   aura-exec-runner    — re-export shell over aura-exec-tools + conflict + isolation
    ("aura-exec-conflict", "exec"),
    ("aura-exec-isolation", "exec"),
    ("aura-exec-policy", "exec"),
    ("aura-exec-sandbox", "exec"),
    ("aura-exec-tools", "exec"),
    ("aura-exec-runner", "exec"),
    // Phase 3 model + context layer renames. The original
    // `aura-<name>` crates are kept as compatibility shells that
    // re-export through the layered `aura-<layer>-<name>` crate.
    // The `aura-<layer>-<name>` names match the
    // `aura-<layer>-<rest>` auto-classification convention exactly,
    // so they could theoretically be omitted from this table, but
    // we list them explicitly for clarity and to make Phase 6a
    // edits surgical.
    ("aura-compaction", "context"),
    ("aura-context-compaction", "context"),
    ("aura-reasoner", "model"),
    ("aura-model-reasoner", "model"),
    ("aura-memory", "context"),
    ("aura-context-memory", "context"),
    ("aura-skills", "context"),
    ("aura-context-skills", "context"),
    ("aura-prompts", "context"),
    ("aura-context-prompts", "context"),
    // Phase 6a renamed `aura-kernel` to `aura-agent-kernel` and
    // converted the original crate into a re-export shell.
    ("aura-kernel", "agent"),
    ("aura-agent-kernel", "agent"),
    ("aura-agent-loop", "agent"),
    ("aura-agent-subagent", "agent"),
    ("aura-terminal", "surface"),
    ("aura-agent", "agent"),
    // Phase 3 placeholder, populated in Phase 6a.
    ("aura-agent-steering", "agent"),
    ("aura-auth", "core"),
    ("aura-automaton", "surface"),
    ("aura-runtime", "surface"),
    ("aura-protocol", "core"),
    // Phase 4b plugin layer:
    //   aura-plugin-api  — in-process contributor traits (first-party only)
    //   aura-plugin-core — declarative manifest + install + cache + marketplace
    ("aura-plugin-api", "plugin"),
    ("aura-plugin-core", "plugin"),
    // Phase 4c plugin runtime surfaces:
    //   aura-plugin-hooks      — HookEngine + 10 Codex/Claude events
    //   aura-plugin-mcp        — stdio JSON-RPC client + connection manager
    //   aura-plugin-connectors — registry of plugin-contributed endpoints
    ("aura-plugin-hooks", "plugin"),
    ("aura-plugin-mcp", "plugin"),
    ("aura-plugin-connectors", "plugin"),
    // Phase 7a fleet layer (5 new crates):
    //   aura-fleet-registry — in-memory agent slot directory
    //   aura-fleet-quota    — tracking-only quota pool (Phase 7b enforces)
    //   aura-fleet-spawn    — spawn mechanics + per-parent audit lease
    //   aura-fleet-dispatch — Phase 7a single-source job adapter
    //   aura-fleet-daemon   — composition root for the four above
    //
    // Fleet sits ABOVE agent and BELOW surface in the layer stack.
    // Fleet crates may depend on agent/exec/plugin/context/model/
    // store/config/core crates (downward); agent crates MUST NOT
    // depend on fleet crates (upward). The auto-classification
    // already maps `aura-fleet-*` correctly via `LAYER_ORDER`, but
    // the explicit entries below keep the table self-documenting.
    ("aura-fleet-registry", "fleet"),
    ("aura-fleet-quota", "fleet"),
    ("aura-fleet-spawn", "fleet"),
    ("aura-fleet-dispatch", "fleet"),
    ("aura-fleet-mailbox", "fleet"),
    ("aura-fleet-daemon", "fleet"),
];

#[test]
fn warn_on_upward_layer_dependencies() {
    let workspace_root = workspace_root();
    let crates_dir = workspace_root.join("crates");

    let mut layer_map: BTreeMap<String, &'static str> = BTreeMap::new();
    let mut manifests: Vec<(String, PathBuf)> = Vec::new();

    let entries = match fs::read_dir(&crates_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("LAYER WARN: cannot enumerate crates dir: {err}");
            return;
        }
    };
    for entry in entries.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&manifest) else {
            continue;
        };
        let Some(name) = parse_package_name(&contents) else {
            continue;
        };
        if let Some(layer) = layer_for(&name) {
            layer_map.insert(name.clone(), layer);
        }
        manifests.push((name, manifest));
    }

    let mut warnings = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for (consumer, manifest) in &manifests {
        let Ok(contents) = fs::read_to_string(manifest) else {
            continue;
        };
        let deps = parse_workspace_dep_names(&contents, &layer_map);
        let Some(consumer_layer) = layer_map.get(consumer.as_str()) else {
            continue;
        };
        let consumer_rank = rank_of(consumer_layer);
        for dep in deps {
            let Some(dep_layer) = layer_map.get(dep.as_str()) else {
                continue;
            };
            let dep_rank = rank_of(dep_layer);
            if dep_rank > consumer_rank {
                let is_warn_only = WARN_ONLY_UPWARD_EDGES
                    .iter()
                    .any(|(c, d)| *c == consumer.as_str() && *d == dep.as_str());
                if is_warn_only {
                    eprintln!(
                        "LAYER WARN: {consumer} ({consumer_layer}) depends on {dep} \
                         ({dep_layer}) — upward edge (allowlisted, see WARN_ONLY_UPWARD_EDGES)"
                    );
                    warnings += 1;
                } else {
                    failures.push(format!(
                        "{consumer} ({consumer_layer}) depends on {dep} ({dep_layer}) — \
                         upward edge not in WARN_ONLY_UPWARD_EDGES allowlist"
                    ));
                }
            }
        }
    }

    eprintln!(
        "LAYER INFO: {} crates classified, {warnings} allowlisted warning(s), {} new violation(s)",
        layer_map.len(),
        failures.len()
    );

    assert!(
        failures.is_empty(),
        "Layer boundary violations:\n  - {}",
        failures.join("\n  - ")
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn layer_for(name: &str) -> Option<&'static str> {
    if let Some((_, layer)) = KNOWN_CRATES.iter().find(|(c, _)| *c == name) {
        if layer.is_empty() {
            return None;
        }
        return Some(*layer);
    }
    if let Some(rest) = name.strip_prefix("aura-") {
        let head = rest.split('-').next().unwrap_or("");
        if LAYER_ORDER.contains(&head) {
            return Some(LAYER_ORDER.iter().copied().find(|l| *l == head).unwrap());
        }
    }
    None
}

fn rank_of(layer: &str) -> usize {
    LAYER_ORDER.iter().position(|l| *l == layer).unwrap_or(0)
}

/// Extract the `name` field under `[package]`.
fn parse_package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package {
            if let Some(rest) = trimmed.strip_prefix("name") {
                let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
                let rest = rest.trim();
                if let Some(stripped) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                    return Some(stripped.to_string());
                }
            }
        }
    }
    None
}

/// Return the set of workspace-member dep names referenced by any
/// dependency table in this manifest. We accept either:
///
/// - `dep_name = { path = "../<crate>" }` — the dep name is the key
///   and we use it directly.
/// - `dep_name = "x.y"` form when `dep_name` exists in `layer_map`.
fn parse_workspace_dep_names(
    toml: &str,
    layer_map: &BTreeMap<String, &'static str>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut in_deps_table = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if let Some(section) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            // [dependencies], [dev-dependencies], [build-dependencies]
            // [target.'cfg(...)'.dependencies], etc.
            in_deps_table = section.ends_with("dependencies");
            continue;
        }
        if !in_deps_table || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // dep_name = ...
        let Some(eq_idx) = trimmed.find('=') else {
            continue;
        };
        let raw_name = trimmed[..eq_idx].trim();
        let name = raw_name
            .trim_matches(|c: char| c == '"' || c == ' ')
            .to_string();
        if name.is_empty() {
            continue;
        }
        if layer_map.contains_key(&name) && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}
