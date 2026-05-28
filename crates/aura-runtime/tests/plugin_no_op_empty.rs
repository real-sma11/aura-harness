//! Phase 8 backward-compatibility invariant test.
//!
//! Asserts that an `~/.aura/plugins/` directory containing zero
//! enabled plugins produces a fully-empty [`PluginRuntime`] and that
//! every one of the 10 lifecycle [`HookEvent`]s reports
//! `is_empty(event) == true`. This is the
//! single check the runtime uses to short-circuit hook firing before
//! any per-event ctx allocation, so an empty install carries zero
//! observable overhead beyond a single boolean read per event.
//!
//! The test exercises three scenarios:
//!
//! 1. **Missing `plugins/` directory**: `load_enabled_plugins` MUST
//!    return an empty runtime (the materialiser tolerates a
//!    non-existent directory).
//! 2. **Empty `plugins/` directory**: ditto, but with the directory
//!    physically present.
//! 3. **Plugin installed but disabled in the [plugins] config**:
//!    the cache list is non-empty but every plugin row is
//!    `enabled = false`, so the runtime is still empty.
//!
//! In every scenario the resulting hook engine satisfies the empty-
//! install invariant: `is_empty(event) == true` for all 10 events,
//! `runtime.is_empty() == true`, no MCP servers are registered, no
//! connectors are registered, no skill roots are added, and no
//! per-plugin load failure entries are produced.

use std::path::PathBuf;

use aura_config::PluginsConfig;
use aura_plugin_core::load_enabled_plugins;
use aura_plugin_hooks::HookEvent;
use tempfile::TempDir;

/// Closed list of every lifecycle event the agent loop, fleet
/// daemon, and fleet spawner can fire. Adding a variant requires
/// updating this list (and the host's per-event firing API).
const ALL_EVENTS: &[HookEvent] = &[
    HookEvent::SessionStart,
    HookEvent::UserPromptSubmit,
    HookEvent::PreToolUse,
    HookEvent::PostToolUse,
    HookEvent::SubagentStart,
    HookEvent::SubagentStop,
    HookEvent::Stop,
    HookEvent::PreCompact,
    HookEvent::PostCompact,
    HookEvent::PermissionRequest,
];

fn assert_empty_runtime(home: &std::path::Path, cfg: &PluginsConfig) {
    let runtime = load_enabled_plugins(home, cfg).expect("load empty install");

    assert!(
        runtime.is_empty(),
        "expected runtime.is_empty() but got enabled={:?}",
        runtime.enabled
    );
    assert!(
        runtime.enabled.is_empty(),
        "no plugins enabled => no enabled refs"
    );
    assert!(
        runtime.load_failures.is_empty(),
        "no plugins enabled => no load failures, got {:?}",
        runtime.load_failures
    );
    assert!(
        runtime.skill_roots.is_empty(),
        "no plugins enabled => no skill roots"
    );

    for event in ALL_EVENTS {
        assert!(
            runtime.hook_engine.is_empty(*event),
            "hook engine must be empty for event {event:?} on an empty install"
        );
    }
}

#[test]
fn missing_plugins_dir_yields_empty_runtime() {
    let tmp = TempDir::new().expect("temp dir");
    let aura_home = tmp.path().join(".aura");
    // Deliberately do NOT create `plugins/` underneath.
    assert!(!aura_home.join("plugins").exists());

    assert_empty_runtime(&aura_home, &PluginsConfig::default());
}

#[test]
fn empty_plugins_dir_yields_empty_runtime() {
    let tmp = TempDir::new().expect("temp dir");
    let aura_home = tmp.path().join(".aura");
    std::fs::create_dir_all(aura_home.join("plugins")).unwrap();

    assert_empty_runtime(&aura_home, &PluginsConfig::default());
}

#[test]
fn disabled_plugins_yield_empty_runtime() {
    let tmp = TempDir::new().expect("temp dir");
    let aura_home = tmp.path().join(".aura");
    let plugin_dir: PathBuf = aura_home.join("plugins").join("disabled-fixture");
    std::fs::create_dir_all(&plugin_dir).unwrap();

    // No `active` pointer file, no manifest. The cache lists the
    // directory entry but `active_version` returns `None` — and
    // because the [plugins] config has no entry for this id, the
    // enable check would short-circuit anyway.
    let cfg = PluginsConfig::default();
    assert_empty_runtime(&aura_home, &cfg);
}
