//! Phase 4c manual fire-event smoke test.
//!
//! Verifies the engine spawns a registered hook against a real OS
//! process, returns a `fire` summary, and that the
//! [`HookFiringContext::env_vars`] payload includes the
//! `CODEX_PLUGIN_ROOT` / `CLAUDE_PLUGIN_ROOT` compatibility aliases.
//!
//! The hook process itself is a tiny no-op script that exits 0 — we
//! deliberately don't try to assert on observed env vars inside the
//! child process because round-tripping env through a child exit code
//! / stdout adds OS-dependent shell quoting fragility. The unit tests
//! in [`aura_plugin_hooks::context`] cover the env shape directly.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aura_core::AgentId;
use aura_plugin_hooks::{HookEngine, HookEvent, HookFiringContext, RegisteredHook};
use tempfile::TempDir;

#[cfg(unix)]
fn noop_script(dir: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("noop.sh");
    std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[cfg(windows)]
fn noop_script(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("noop.cmd");
    std::fs::write(&p, "@echo off\r\nexit /b 0\r\n").unwrap();
    p
}

#[test]
fn engine_fires_event_with_no_registrations_returns_empty_summary() {
    let engine = HookEngine::default();
    let ctx = HookFiringContext {
        plugin_root: std::env::temp_dir(),
        event: HookEvent::SessionStart,
        agent_id: AgentId::new([0u8; 32]),
        session_id: "sess-0".into(),
        turn_id: None,
        extra: BTreeMap::new(),
    };
    let summary = engine.fire(&ctx).expect("fire ok");
    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.failed, 0);
}

#[test]
fn engine_fires_event_with_env_injection() {
    let dir = TempDir::new().expect("temp dir");
    let script = noop_script(dir.path());

    let mut engine = HookEngine::default();
    engine.register(RegisteredHook {
        plugin_id: "test-plug".into(),
        event: HookEvent::SessionStart,
        command: script.to_string_lossy().into_owned(),
        args: vec![],
        plugin_root: dir.path().to_path_buf(),
        env: BTreeMap::new(),
    });

    let ctx = HookFiringContext {
        plugin_root: dir.path().to_path_buf(),
        event: HookEvent::SessionStart,
        agent_id: AgentId::new([1u8; 32]),
        session_id: "sess-1".into(),
        turn_id: None,
        extra: BTreeMap::new(),
    };

    let summary = engine.fire(&ctx).expect("fire ok");
    assert_eq!(summary.succeeded, 1, "noop script must exit 0");
    assert_eq!(summary.failed, 0);
}

#[test]
fn env_vars_include_codex_and_claude_aliases() {
    let dir = TempDir::new().unwrap();
    let ctx = HookFiringContext {
        plugin_root: dir.path().to_path_buf(),
        event: HookEvent::PreToolUse,
        agent_id: AgentId::new([2u8; 32]),
        session_id: "sess-2".into(),
        turn_id: Some("turn-1".into()),
        extra: BTreeMap::new(),
    };
    let env = ctx.env_vars();
    assert!(env.contains_key("AURA_PLUGIN_ROOT"));
    assert!(env.contains_key("CODEX_PLUGIN_ROOT"));
    assert!(env.contains_key("CLAUDE_PLUGIN_ROOT"));
    assert_eq!(
        env.get("AURA_EVENT").map(String::as_str),
        Some("pre_tool_use")
    );
    assert_eq!(env.get("AURA_TURN_ID").map(String::as_str), Some("turn-1"));
}

#[test]
fn engine_records_failed_hook_in_summary() {
    let mut engine = HookEngine::default();
    // Register a deliberately bogus command. The spawn will fail (no
    // such binary on the OS PATH); the summary must show 1 failure
    // and no success, and the engine must NOT return an Err.
    engine.register(RegisteredHook {
        plugin_id: "broken".into(),
        event: HookEvent::SessionStart,
        command: "this-binary-does-not-exist-aura-test".into(),
        args: vec![],
        plugin_root: std::env::temp_dir(),
        env: BTreeMap::new(),
    });
    let ctx = HookFiringContext {
        plugin_root: std::env::temp_dir(),
        event: HookEvent::SessionStart,
        agent_id: AgentId::new([3u8; 32]),
        session_id: "sess-3".into(),
        turn_id: None,
        extra: BTreeMap::new(),
    };
    let summary = engine.fire(&ctx).expect("fire ok even when spawn fails");
    assert_eq!(summary.succeeded, 0);
    assert_eq!(summary.failed, 1);
}
