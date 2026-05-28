//! Phase 8 end-to-end fixture-plugin test.
//!
//! Materialises a synthetic plugin under a temporary
//! `~/.aura/plugins/test-plugin/<version>/` tree and asserts the
//! load pipeline:
//!
//! 1. Loads the plugin's skill directory (skill markdown discoverable
//!    via the `[[contributes.skills]]` `path`).
//! 2. Registers the plugin's MCP server config (`server_id` retrievable
//!    by id).
//! 3. Registers the plugin's hook (the registered command + plugin
//!    root match the manifest, and the engine reports
//!    `is_empty(PreToolUse) == false`).
//! 4. Registers the plugin's connector (the registry returns the
//!    plugin-supplied entry by id).
//! 5. Fires `PreToolUse` through the [`PluginHookHost`] and asserts
//!    the side-effect file exists with the expected content.
//!
//! The test runs in-process and does not spawn any external server;
//! the MCP server config + connector entry are validated by the
//! manager / registry's `get` APIs without launching the subprocess.
//! The PreToolUse hook script writes a deterministic side-effect
//! file so the assertion is robust on both Unix and Windows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_config::{PluginConfig, PluginsConfig, PluginsTable};
use aura_plugin_core::load_enabled_plugins;
use aura_plugin_hooks::{HookEvent, PluginHookHost};
use tempfile::TempDir;

#[cfg(unix)]
fn write_hook_script(dir: &Path, target: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("on-pre-tool-use.sh");
    let target_str = target.to_string_lossy();
    let script =
        format!("#!/bin/sh\nprintf 'fired: %s' \"$AURA_EVENT\" > \"{target_str}\"\nexit 0\n");
    std::fs::write(&p, script).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[cfg(windows)]
fn write_hook_script(dir: &Path, target: &Path) -> PathBuf {
    let p = dir.join("on-pre-tool-use.cmd");
    let target_str = target.to_string_lossy();
    let script = format!(
        "@echo off\r\n>\"{target_str}\" echo fired: %AURA_EVENT%\r\nexit /b 0\r\n",
        target_str = target_str
    );
    std::fs::write(&p, script).unwrap();
    p
}

/// Build the fixture plugin tree. Returns the resolved
/// `aura_home`, plugin id, and the side-effect file the hook
/// script will write into.
fn install_fixture_plugin(tmp: &Path) -> (PathBuf, String, PathBuf) {
    let aura_home = tmp.join(".aura");
    let plugin_id = "test-plugin".to_string();
    let version = "0.1.0";
    let plugin_dir = aura_home.join("plugins").join(&plugin_id).join(version);
    std::fs::create_dir_all(&plugin_dir).unwrap();

    // Skill: a markdown body under `skills/`.
    std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();
    std::fs::write(
        plugin_dir.join("skills").join("hello.md"),
        "# hello\n\nfixture skill body\n",
    )
    .unwrap();

    // Hook script that writes a side-effect file.
    let side_effect = tmp.join("hook-fired.txt");
    let script_path = write_hook_script(&plugin_dir, &side_effect);
    let script_rel = script_path
        .strip_prefix(&plugin_dir)
        .unwrap()
        .to_string_lossy()
        .into_owned()
        .replace('\\', "/");

    // Use a real OS binary as the MCP server stub. The
    // [`McpConnectionManager::register`] path immediately spawns
    // the child; it doesn't JSON-RPC until a caller invokes
    // `with_client`. A short-lived sleep is a portable stand-in
    // that lets the registration succeed without shipping a real
    // fixture binary.
    let mcp_block = if cfg!(windows) {
        r#"[[contributes.mcp]]
server_id = "test-mcp"
command = "cmd.exe"
args = ["/C", "timeout /T 5 /NOBREAK > nul"]
"#
    } else {
        r#"[[contributes.mcp]]
server_id = "test-mcp"
command = "/bin/sh"
args = ["-c", "sleep 5"]
"#
    };

    let manifest = format!(
        r#"manifest_version = "v1"
id = "{plugin_id}"
version = "{version}"
name = "Test Plugin"
description = "Phase 8 e2e fixture"

[[contributes.skills]]
id = "hello"
path = "./skills/hello.md"

[[contributes.hooks]]
event = "PreToolUse"
command = "./{script_rel}"
args = []

{mcp_block}
[[contributes.connectors]]
id = "test-connector"
endpoint = "noop://"
"#
    );
    std::fs::write(plugin_dir.join(".aura-plugin.toml"), manifest).unwrap();

    // Active version pointer (the cache uses a bare `active`
    // filename, not a dotfile).
    std::fs::write(
        aura_home.join("plugins").join(&plugin_id).join("active"),
        version,
    )
    .unwrap();

    (aura_home, plugin_id, side_effect)
}

fn enabled_plugins_config(plugin_id: &str) -> PluginsConfig {
    let mut table = BTreeMap::new();
    table.insert(
        plugin_id.to_string(),
        PluginConfig {
            enabled: true,
            trusted: true,
            version: None,
        },
    );
    PluginsConfig {
        table: PluginsTable(table),
    }
}

#[test]
fn e2e_fixture_plugin_materialises_all_contributions() {
    let tmp = TempDir::new().expect("tmp");
    let (aura_home, plugin_id, side_effect) = install_fixture_plugin(tmp.path());
    let cfg = enabled_plugins_config(&plugin_id);

    let runtime = load_enabled_plugins(&aura_home, &cfg).expect("plugin runtime materialises");

    assert_eq!(
        runtime.enabled.len(),
        1,
        "fixture plugin should be enabled: failures={:?}",
        runtime.load_failures
    );
    assert!(
        runtime.load_failures.is_empty(),
        "no failures expected, got {:?}",
        runtime.load_failures
    );

    // (1) Skill: the plugin contributed `./skills/hello.md`.
    //     The materialiser collects the *parent* directory as a
    //     skill root (the loader downstream walks the directory
    //     for *.md skill files).
    assert_eq!(
        runtime.skill_roots.len(),
        1,
        "expected exactly one skill root from the fixture"
    );
    let skill_root = &runtime.skill_roots[0];
    assert!(
        skill_root.join("hello.md").exists(),
        "skill markdown must exist under the registered root"
    );

    // (2) MCP server: `test-mcp` must be registered with the
    //     manager. We probe via `contains` rather than spawning the
    //     subprocess (the fixture doesn't ship a real binary).
    assert!(
        runtime.mcp.contains("test-mcp"),
        "MCP server `test-mcp` must be registered"
    );
    let server_ids = runtime.mcp.server_ids();
    assert!(
        server_ids.iter().any(|s| s == "test-mcp"),
        "server_ids() should include the fixture's id, got {server_ids:?}"
    );

    // (3) Hook: engine reports a registered handler for
    //     `PreToolUse`.
    assert!(
        !runtime.hook_engine.is_empty(HookEvent::PreToolUse),
        "PreToolUse hook must be registered after materialisation"
    );
    // The other 9 events stay empty — the fixture only wires
    // PreToolUse.
    for event in [
        HookEvent::SessionStart,
        HookEvent::UserPromptSubmit,
        HookEvent::PostToolUse,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::Stop,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::PermissionRequest,
    ] {
        assert!(
            runtime.hook_engine.is_empty(event),
            "fixture only wires PreToolUse; {event:?} must remain empty"
        );
    }

    // (4) Connector: registry returns the plugin-supplied entry.
    let connector = runtime
        .connectors
        .get("test-connector")
        .expect("connector `test-connector` must be registered");
    assert_eq!(connector.endpoint, "noop://");
    assert_eq!(connector.plugin_id, "test-plugin");

    // (5) Fire the PreToolUse hook and assert the side-effect
    //     file exists with the expected content. We use the
    //     production `PluginHookHost` so the env-injection /
    //     scrubbing pipeline is exercised end-to-end.
    let host = PluginHookHost {
        engine: Arc::clone(&runtime.hook_engine),
        aura_home: aura_home.clone(),
        session_id: "sess-e2e".to_string(),
        agent_id: "agent-e2e".to_string(),
        parent_agent_id: None,
    };
    let outcome = host.fire_pre_tool_use("read_file", "{\"path\":\"x\"}", "call-1");
    assert!(
        outcome.ran >= 1,
        "PreToolUse hook should have run at least once: {outcome:?}"
    );
    assert_eq!(
        outcome.timed_out, 0,
        "fixture hook must not time out: {outcome:?}"
    );

    // The hook script writes the side-effect file
    // synchronously; on Windows the cmd shim sometimes
    // flushes a tick later, so we tolerate a tiny grace window.
    for _ in 0..50 {
        if side_effect.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        side_effect.exists(),
        "PreToolUse hook should have written {}",
        side_effect.display()
    );
    let body = std::fs::read_to_string(&side_effect).unwrap();
    assert!(
        body.contains("fired"),
        "hook side-effect file should contain 'fired', got {body:?}"
    );
    assert!(
        body.contains("pre_tool_use"),
        "hook side-effect file should record AURA_EVENT='pre_tool_use', got {body:?}"
    );
}
