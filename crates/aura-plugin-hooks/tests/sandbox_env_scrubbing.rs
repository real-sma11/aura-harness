//! Phase 8 sandbox env-scrubbing integration test.
//!
//! Spawns a real OS process via [`aura_plugin_hooks::sandbox::scrubbed_env`]
//! that dumps its env to stdout and asserts:
//!
//! 1. Secret patterns from [`SECRET_PATTERNS`] / [`SECRET_NAMES`] are
//!    absent in the child's env.
//! 2. The canonical Aura vars (`AURA_HOME`, `PLUGIN_ROOT`,
//!    `AURA_SESSION_ID`, `AURA_AGENT_ID`, `AURA_PARENT_AGENT_ID`,
//!    `AURA_EVENT`) are present with the expected values.
//! 3. The Codex/Claude compat aliases (`CODEX_HOME`,
//!    `CLAUDE_PLUGIN_ROOT`, `CODEX_SESSION_ID`, `CODEX_AGENT_ID`,
//!    `CODEX_PARENT_ID`, `CODEX_EVENT`) are present and mirror the
//!    Aura values.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use aura_plugin_hooks::sandbox::{scrubbed_env, InjectedEnv, ENV_TEST_LOCK};

#[cfg(unix)]
fn dump_env_script(dir: &std::path::Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("dump-env.sh");
    std::fs::write(&p, "#!/bin/sh\nenv\n").unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[cfg(windows)]
fn dump_env_script(dir: &std::path::Path) -> PathBuf {
    let p = dir.join("dump-env.cmd");
    // `set` lists every env var as `KEY=VALUE` lines, one per line —
    // exactly the shape `parse_env_dump` expects.
    std::fs::write(&p, "@echo off\r\nset\r\n").unwrap();
    p
}

fn run_dump(dir: &std::path::Path, env: &BTreeMap<String, String>) -> String {
    let script = dump_env_script(dir);
    let mut cmd = Command::new(if cfg!(windows) { "cmd.exe" } else { "/bin/sh" });
    if cfg!(windows) {
        cmd.args(["/C", script.to_string_lossy().as_ref()]);
    } else {
        cmd.arg(script.as_os_str());
    }
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn dump-env");
    assert!(out.status.success(), "dump-env exited non-zero");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn parse_env_dump(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(p) = self.previous.take() {
            std::env::set_var(self.key, p);
        } else {
            std::env::remove_var(self.key);
        }
    }
}
fn set_test_env(key: &'static str, value: &str) -> EnvVarGuard {
    let previous = std::env::var(key).ok();
    std::env::set_var(key, value);
    EnvVarGuard { key, previous }
}

fn injected(home: &std::path::Path, root: &std::path::Path) -> InjectedEnv {
    InjectedEnv {
        event_name: "pre_tool_use",
        aura_home: home.to_path_buf(),
        plugin_root: root.to_path_buf(),
        session_id: "sess-X".into(),
        agent_id: "agent-Y".into(),
        parent_agent_id: Some("parent-Z".into()),
        extra: BTreeMap::new(),
    }
}

#[test]
fn sandbox_env_scrubbing_removes_secrets_and_injects_canonicals() {
    let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Plant a representative set of secrets in the parent env.
    let _g_aws = set_test_env("AWS_SECRET_ACCESS_KEY", "akia-fake-leak");
    let _g_gcp = set_test_env("GCP_SERVICE_ACCOUNT", "gcp-fake-leak");
    let _g_azure = set_test_env("AZURE_CLIENT_SECRET", "azure-fake-leak");
    let _g_openai = set_test_env("OPENAI_API_KEY", "sk-fake-leak");
    let _g_anthropic = set_test_env("ANTHROPIC_API_KEY", "sk-ant-fake-leak");
    let _g_google = set_test_env("GOOGLE_API_KEY", "gak-fake-leak");
    let _g_kube = set_test_env("KUBECONFIG", "/etc/kube/config");
    let _g_ssh = set_test_env("SSH_AUTH_SOCK", "/tmp/ssh.sock");
    let _g_docker = set_test_env("DOCKER_HOST", "tcp://example:1234");
    let _g_token = set_test_env("MY_GITHUB_TOKEN", "ghp-fake-leak");
    let _g_secret = set_test_env("DB_HMAC_SECRET", "fake-leak");
    let _g_password = set_test_env("ROOT_PASSWORD", "fake-leak");
    let _g_key = set_test_env("APP_SIGNING_KEY", "fake-leak");

    let dir = tempfile::TempDir::new().unwrap();
    let aura_home = dir.path().join(".aura");
    let plugin_root = aura_home.join("plugins").join("demo").join("1.0.0");
    std::fs::create_dir_all(&plugin_root).unwrap();

    let env = scrubbed_env(&injected(&aura_home, &plugin_root));
    let dump = run_dump(dir.path(), &env);
    let observed = parse_env_dump(&dump);

    // (a) secrets must NOT appear in the child env.
    for forbidden in [
        "AWS_SECRET_ACCESS_KEY",
        "GCP_SERVICE_ACCOUNT",
        "AZURE_CLIENT_SECRET",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GOOGLE_API_KEY",
        "KUBECONFIG",
        "SSH_AUTH_SOCK",
        "DOCKER_HOST",
        "MY_GITHUB_TOKEN",
        "DB_HMAC_SECRET",
        "ROOT_PASSWORD",
        "APP_SIGNING_KEY",
    ] {
        assert!(
            !observed.contains_key(forbidden),
            "secret env var '{forbidden}' leaked into hook child env"
        );
    }

    // (b) canonical Aura vars present with expected values.
    let aura_home_str = aura_home.to_string_lossy().into_owned();
    let plugin_root_str = plugin_root.to_string_lossy().into_owned();
    assert_eq!(observed.get("AURA_HOME"), Some(&aura_home_str));
    assert_eq!(observed.get("PLUGIN_ROOT"), Some(&plugin_root_str));
    assert_eq!(
        observed.get("AURA_SESSION_ID").map(String::as_str),
        Some("sess-X")
    );
    assert_eq!(
        observed.get("AURA_AGENT_ID").map(String::as_str),
        Some("agent-Y")
    );
    assert_eq!(
        observed.get("AURA_PARENT_AGENT_ID").map(String::as_str),
        Some("parent-Z")
    );
    assert_eq!(
        observed.get("AURA_EVENT").map(String::as_str),
        Some("pre_tool_use")
    );

    // (c) Codex / Claude aliases mirror the Aura values.
    assert_eq!(observed.get("CODEX_HOME"), Some(&aura_home_str));
    assert_eq!(observed.get("CLAUDE_PLUGIN_ROOT"), Some(&plugin_root_str));
    assert_eq!(
        observed.get("CODEX_SESSION_ID").map(String::as_str),
        Some("sess-X")
    );
    assert_eq!(
        observed.get("CODEX_AGENT_ID").map(String::as_str),
        Some("agent-Y")
    );
    assert_eq!(
        observed.get("CODEX_PARENT_ID").map(String::as_str),
        Some("parent-Z")
    );
    assert_eq!(
        observed.get("CODEX_EVENT").map(String::as_str),
        Some("pre_tool_use")
    );
}
