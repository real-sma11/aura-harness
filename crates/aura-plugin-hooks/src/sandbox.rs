//! Env-var scrubbing for hook subprocess sandbox.
//!
//! ## Invariants ([rules.md §13])
//!
//! - When launching a hook command we MUST NOT inherit the operator's
//!   cloud-provider / model credentials. Hook scripts run in the
//!   plugin author's trust domain; leaking operator tokens into a
//!   third-party process is a security regression.
//! - The scrubbed env starts from the parent env, then drops every
//!   variable matching a secret pattern (see [`SECRET_PATTERNS`])
//!   and every variable on the explicit deny list (see
//!   [`SECRET_NAMES`]). Whatever remains is overlaid with the
//!   per-firing canonical Aura vars + Codex/Claude compat aliases.
//! - This module mutates only the **child** env via the returned map.
//!   It does NOT call [`std::env::set_var`] / [`remove_var`]; the
//!   parent process's env is untouched.
//!
//! ## Codex / Claude compat aliases
//!
//! Phase 8 ships the documented alias table for cross-marketplace
//! plugin packages. Both names are injected so a hook script ported
//! from Codex / Claude works unchanged. The Codex aliases are
//! documented as "V1 compat — to be removed in V2" — they are not
//! a permanent API.
//!
//! | Codex var          | Aura var                | Notes                                   |
//! |--------------------|-------------------------|-----------------------------------------|
//! | `CODEX_HOME`       | `AURA_HOME`             | Codex var injected as alias for V1 only |
//! | `CLAUDE_PLUGIN_ROOT` | `PLUGIN_ROOT`         | Both injected for cross-marketplace compat |
//! | `CODEX_SESSION_ID` | `AURA_SESSION_ID`       | Both injected                           |
//! | `CODEX_AGENT_ID`   | `AURA_AGENT_ID`         | Both injected                           |
//! | `CODEX_PARENT_ID`  | `AURA_PARENT_AGENT_ID`  | Both injected                           |
//! | `CODEX_EVENT`      | `AURA_EVENT`            | Both injected                           |

use std::collections::BTreeMap;
use std::path::Path;

/// Explicit deny list of env var names that must never leak into a
/// hook subprocess. The list captures common credential names that
/// don't match the suffix patterns in [`SECRET_PATTERNS`].
pub const SECRET_NAMES: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "KUBECONFIG",
    "SSH_AUTH_SOCK",
    // Aura-managed credentials that must never reach hook subprocesses
    "AURA_AUTH_TOKEN",
    "AURA_REFRESH_TOKEN",
];

/// Suffix / prefix patterns that mark a var as a secret. Any env
/// var whose name matches ANY of these patterns is scrubbed.
///
/// The patterns are simple `*pat*` / `pat*` / `*pat` shapes
/// (deliberately not regex — keeping the match logic fast and
/// auditable).
///
/// Matching rules:
/// - `"AWS_*"` — name starts with `"AWS_"`.
/// - `"*_TOKEN"` — name ends with `"_TOKEN"`.
/// - `"*_KEY"` — name ends with `"_KEY"`.
pub const SECRET_PATTERNS: &[&str] = &[
    "AWS_*",
    "GCP_*",
    "AZURE_*",
    "DOCKER_*",
    "*_TOKEN",
    "*_SECRET",
    "*_PASSWORD",
    "*_KEY",
];

/// Per-firing identifiers used to construct the canonical env var
/// payload. Mirrors the [`crate::ctx::CtxMeta`] shape but accepts
/// `None` for parent agent (root agents).
#[derive(Clone, Debug)]
pub struct InjectedEnv {
    /// Snake_case event name (e.g. `"pre_tool_use"`).
    pub event_name: &'static str,
    /// Resolved `AURA_HOME` path.
    pub aura_home: std::path::PathBuf,
    /// Plugin install root (`AURA_HOME/plugins/<name>`).
    pub plugin_root: std::path::PathBuf,
    /// Session id firing the hook.
    pub session_id: String,
    /// Agent id firing the hook.
    pub agent_id: String,
    /// Parent agent id (empty string for root agents).
    pub parent_agent_id: Option<String>,
    /// Extra firing-site env vars (already merged AFTER the canonical
    /// set in [`scrubbed_env`]).
    pub extra: BTreeMap<String, String>,
}

/// Build the env map for a hook subprocess.
///
/// Steps:
///
/// 1. Inherit the parent env, dropping every name matching a secret
///    pattern in [`SECRET_PATTERNS`] or appearing in [`SECRET_NAMES`].
/// 2. Inject the canonical Aura variables (`AURA_HOME`,
///    `PLUGIN_ROOT`, `AURA_SESSION_ID`, `AURA_AGENT_ID`,
///    `AURA_PARENT_AGENT_ID`, `AURA_EVENT`).
/// 3. Inject the Codex / Claude compat aliases (`CODEX_HOME`,
///    `CODEX_SESSION_ID`, `CODEX_AGENT_ID`, `CODEX_PARENT_ID`,
///    `CODEX_EVENT`, `CLAUDE_PLUGIN_ROOT`).
/// 4. Merge `injected.extra` last so the firing site can supplement
///    canonical names if it really wants to.
#[must_use]
pub fn scrubbed_env(injected: &InjectedEnv) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();

    for (k, v) in std::env::vars() {
        if is_secret_name(&k) {
            continue;
        }
        env.insert(k, v);
    }

    overlay_canonical(&mut env, injected);

    for (k, v) in &injected.extra {
        env.insert(k.clone(), v.clone());
    }

    env
}

/// Overlay the canonical Aura + Codex/Claude alias env vars onto an
/// existing map. Used by [`scrubbed_env`] but kept as a standalone
/// helper for tests and for the legacy [`scrubbed_inherit`] callers.
pub fn overlay_canonical(env: &mut BTreeMap<String, String>, injected: &InjectedEnv) {
    let aura_home = path_to_string(&injected.aura_home);
    let plugin_root = path_to_string(&injected.plugin_root);
    let parent = injected.parent_agent_id.clone().unwrap_or_default();

    env.insert("AURA_HOME".into(), aura_home.clone());
    env.insert("PLUGIN_ROOT".into(), plugin_root.clone());
    env.insert("AURA_SESSION_ID".into(), injected.session_id.clone());
    env.insert("AURA_AGENT_ID".into(), injected.agent_id.clone());
    env.insert("AURA_PARENT_AGENT_ID".into(), parent.clone());
    env.insert("AURA_EVENT".into(), injected.event_name.into());

    env.insert("CODEX_HOME".into(), aura_home);
    env.insert("CLAUDE_PLUGIN_ROOT".into(), plugin_root);
    env.insert("CODEX_SESSION_ID".into(), injected.session_id.clone());
    env.insert("CODEX_AGENT_ID".into(), injected.agent_id.clone());
    env.insert("CODEX_PARENT_ID".into(), parent);
    env.insert("CODEX_EVENT".into(), injected.event_name.into());
}

/// True when a parent-env name should be scrubbed before being
/// inherited by a hook subprocess.
#[must_use]
pub fn is_secret_name(name: &str) -> bool {
    if SECRET_NAMES.contains(&name) {
        return true;
    }
    SECRET_PATTERNS.iter().any(|pat| matches_pattern(pat, name))
}

/// Check whether `name` matches one of the simple wildcard patterns
/// supported by [`SECRET_PATTERNS`]. Supports `prefix*`, `*suffix`,
/// and a literal name with no `*`.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if pattern == name {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    false
}

/// Convert a `Path` to a `String` for env-var injection. Lossy
/// conversion is acceptable here because hook subprocess env-vars
/// must be valid UTF-8 by `std::process::Command` contract on
/// Windows; on Unix non-UTF-8 path components are exceedingly rare.
fn path_to_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// **Legacy** helper retained for the Phase 4c hook-engine call
/// sites. Returns a sandbox env map that inherits the limited safe
/// list of vars (PATH, HOME, locale, etc.) without injecting the
/// canonical hook variables. Callers that fire hooks via the Phase 8
/// pipeline should use [`scrubbed_env`] instead.
#[must_use]
pub fn scrubbed_inherit() -> BTreeMap<String, String> {
    const LEGACY_SAFE: &[&str] = &[
        // Unix / cross-platform minimal:
        "PATH",
        "HOME",
        "USER",
        "USERNAME",
        "TERM",
        "LANG",
        "TMPDIR",
        "TMP",
        "TEMP",
        "TZ",
        "SHELL",
        // Windows minimal:
        "SYSTEMROOT",
        "WINDIR",
        "APPDATA",
        "LOCALAPPDATA",
        "USERPROFILE",
        "COMPUTERNAME",
        "PATHEXT",
        "PROCESSOR_ARCHITECTURE",
    ];
    let mut env = BTreeMap::new();
    for k in LEGACY_SAFE {
        if let Ok(v) = std::env::var(k) {
            env.insert((*k).to_string(), v);
        }
    }
    for (k, v) in std::env::vars() {
        if k.starts_with("LC_") {
            env.insert(k, v);
        }
    }
    env
}

/// Allowlist of env-var names safe to inherit from the parent
/// process via [`scrubbed_inherit`]. Kept `pub` for the Phase 4c
/// integration tests; Phase 8 callers should use [`SECRET_PATTERNS`]
/// + [`SECRET_NAMES`] instead.
pub const SAFE_INHERIT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "USERNAME",
    "TERM",
    "LANG",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TZ",
    "SHELL",
    "SYSTEMROOT",
    "WINDIR",
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "COMPUTERNAME",
    "PATHEXT",
    "PROCESSOR_ARCHITECTURE",
];

/// Process-wide test mutex for tests that need to mutate the parent
/// env around a [`scrubbed_inherit`] / [`scrubbed_env`] probe.
///
/// Exposed `pub` so integration tests under `tests/` can serialise
/// against the same lock as the in-crate unit tests; without
/// serialisation, parallel `cargo test` runs interleave
/// `set_var` / `remove_var` and the assertions become flaky.
pub static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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

    fn injected() -> InjectedEnv {
        InjectedEnv {
            event_name: "pre_tool_use",
            aura_home: PathBuf::from(if cfg!(windows) {
                r"C:\users\u\.aura"
            } else {
                "/home/u/.aura"
            }),
            plugin_root: PathBuf::from(if cfg!(windows) {
                r"C:\users\u\.aura\plugins\demo\1.0.0"
            } else {
                "/home/u/.aura/plugins/demo/1.0.0"
            }),
            session_id: "sess-1".into(),
            agent_id: "agent-1".into(),
            parent_agent_id: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn aws_credentials_are_scrubbed() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = set_test_env("AWS_ACCESS_KEY_ID", "AKIAFAKE");
        let _g2 = set_test_env("AWS_SECRET_ACCESS_KEY", "fakesecret");
        let env = scrubbed_env(&injected());
        assert!(!env.contains_key("AWS_ACCESS_KEY_ID"));
        assert!(!env.contains_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn openai_anthropic_google_keys_scrubbed() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = set_test_env("OPENAI_API_KEY", "sk-fake");
        let _g2 = set_test_env("ANTHROPIC_API_KEY", "sk-ant-fake");
        let _g3 = set_test_env("GOOGLE_API_KEY", "fake");
        let env = scrubbed_env(&injected());
        assert!(!env.contains_key("OPENAI_API_KEY"));
        assert!(!env.contains_key("ANTHROPIC_API_KEY"));
        assert!(!env.contains_key("GOOGLE_API_KEY"));
    }

    #[test]
    fn token_secret_password_key_suffixes_scrubbed() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = set_test_env("MY_API_TOKEN", "x");
        let _g2 = set_test_env("DB_PASSWORD", "y");
        let _g3 = set_test_env("HMAC_SECRET", "z");
        let _g4 = set_test_env("APP_SIGNING_KEY", "w");
        let env = scrubbed_env(&injected());
        assert!(!env.contains_key("MY_API_TOKEN"));
        assert!(!env.contains_key("DB_PASSWORD"));
        assert!(!env.contains_key("HMAC_SECRET"));
        assert!(!env.contains_key("APP_SIGNING_KEY"));
    }

    #[test]
    fn kubeconfig_and_ssh_auth_sock_scrubbed() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g1 = set_test_env("KUBECONFIG", "/etc/k8s.yaml");
        let _g2 = set_test_env("SSH_AUTH_SOCK", "/tmp/ssh-sock");
        let env = scrubbed_env(&injected());
        assert!(!env.contains_key("KUBECONFIG"));
        assert!(!env.contains_key("SSH_AUTH_SOCK"));
    }

    #[test]
    fn canonical_aura_env_injected() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = scrubbed_env(&injected());
        assert!(env.contains_key("AURA_HOME"));
        assert!(env.contains_key("PLUGIN_ROOT"));
        assert_eq!(
            env.get("AURA_SESSION_ID").map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(
            env.get("AURA_AGENT_ID").map(String::as_str),
            Some("agent-1")
        );
        assert_eq!(
            env.get("AURA_PARENT_AGENT_ID").map(String::as_str),
            Some("")
        );
        assert_eq!(
            env.get("AURA_EVENT").map(String::as_str),
            Some("pre_tool_use")
        );
    }

    #[test]
    fn codex_and_claude_aliases_mirror_aura_values() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let env = scrubbed_env(&injected());
        let aura_home = env.get("AURA_HOME").cloned().unwrap_or_default();
        let plugin_root = env.get("PLUGIN_ROOT").cloned().unwrap_or_default();
        assert_eq!(env.get("CODEX_HOME"), Some(&aura_home));
        assert_eq!(env.get("CLAUDE_PLUGIN_ROOT"), Some(&plugin_root));
        assert_eq!(
            env.get("CODEX_SESSION_ID").map(String::as_str),
            Some("sess-1")
        );
        assert_eq!(
            env.get("CODEX_AGENT_ID").map(String::as_str),
            Some("agent-1")
        );
        assert_eq!(env.get("CODEX_PARENT_ID").map(String::as_str), Some(""));
        assert_eq!(
            env.get("CODEX_EVENT").map(String::as_str),
            Some("pre_tool_use")
        );
    }

    #[test]
    fn parent_agent_id_propagates_when_some() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut inj = injected();
        inj.parent_agent_id = Some("parent-99".into());
        let env = scrubbed_env(&inj);
        assert_eq!(
            env.get("AURA_PARENT_AGENT_ID").map(String::as_str),
            Some("parent-99")
        );
        assert_eq!(
            env.get("CODEX_PARENT_ID").map(String::as_str),
            Some("parent-99")
        );
    }

    #[test]
    fn pattern_matching_handles_prefix_and_suffix_wildcards() {
        assert!(matches_pattern("AWS_*", "AWS_REGION"));
        assert!(!matches_pattern("AWS_*", "AZURE_TOKEN"));
        assert!(matches_pattern("*_TOKEN", "GITHUB_TOKEN"));
        assert!(!matches_pattern("*_TOKEN", "TOKENISER"));
        assert!(matches_pattern("FOO", "FOO"));
        assert!(!matches_pattern("FOO", "FOOBAR"));
    }

    #[test]
    fn extra_overlay_merges_last() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut inj = injected();
        inj.extra.insert("MY_PLUGIN_FLAG".into(), "yes".into());
        let env = scrubbed_env(&inj);
        assert_eq!(env.get("MY_PLUGIN_FLAG").map(String::as_str), Some("yes"));
    }

    #[test]
    fn legacy_scrubbed_inherit_drops_aws_creds() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _g = set_test_env("AWS_ACCESS_KEY_ID", "AKIA-FAKE-LEAK-CHECK");
        let env = scrubbed_inherit();
        assert!(!env.contains_key("AWS_ACCESS_KEY_ID"));
    }
}
