//! Env-var scrubbing for hook subprocess sandbox.
//!
//! ## Invariants ([rules.md §13])
//!
//! - When launching a hook command we MUST NOT inherit the operator's
//!   cloud-provider credentials (`AWS_*`, `GCP_*`, `AZURE_*`,
//!   `OPENAI_*`, `ANTHROPIC_*`, `GITHUB_TOKEN`, etc.). Hook scripts
//!   run in the plugin author's trust domain; leaking operator
//!   tokens into a third-party process is a security regression.
//! - The scrubbed env starts **empty** and only contains:
//!     - PATH / HOME / USER / TERM / LANG / LC_* / TMPDIR / TZ /
//!       SHELL (Unix minimal)
//!     - SYSTEMROOT / WINDIR / APPDATA / LOCALAPPDATA / USERPROFILE /
//!       COMPUTERNAME / PATHEXT / PROCESSOR_ARCHITECTURE (Windows
//!       minimal)
//!     - the [`crate::HookFiringContext::env_vars`] payload
//!     - any explicitly-allowlisted vars from the registered hook's
//!       `env` map
//! - This module mutates only the **child** env via the returned map.
//!   It does NOT call [`std::env::set_var`] / [`remove_var`]; the
//!   parent process's env is untouched.

use std::collections::BTreeMap;

/// Allowlist of env-var names safe to inherit from the parent process
/// into a hook subprocess. The list is deliberately conservative —
/// see the module-level invariant.
pub const SAFE_INHERIT: &[&str] = &[
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

/// Build the inherit-safe env map for a hook subprocess.
///
/// Only names in [`SAFE_INHERIT`] plus any `LC_*` locale variant are
/// passed through. Operator secrets (cloud credentials, model API
/// keys, GitHub tokens, …) are dropped on the floor. Callers extend
/// the returned map with the [`crate::HookFiringContext::env_vars`]
/// payload before passing it to [`std::process::Command::env`].
#[must_use]
pub fn scrubbed_inherit() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for k in SAFE_INHERIT {
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

/// Process-wide test mutex for tests that need to mutate the parent
/// env around a [`scrubbed_inherit`] probe. Same pattern as
/// `aura-config::env::ENV_TEST_LOCK`: scoped to `#[cfg(test)]` so
/// production builds carry zero overhead.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Setting a fake cloud-credential env var in the test process
    /// MUST NOT leak through the scrubbed inheritance.
    #[test]
    fn aws_credentials_are_not_inherited() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Set a synthetic AWS key in the test process env. The
        // scrubbed inherit MUST drop it. Restoring on drop avoids
        // leaking state to other tests in the same process.
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
        let key = "AWS_ACCESS_KEY_ID";
        let previous = std::env::var(key).ok();
        std::env::set_var(key, "AKIA-FAKE-LEAK-CHECK");
        let _guard = EnvVarGuard { key, previous };

        let env = scrubbed_inherit();
        assert!(
            !env.contains_key("AWS_ACCESS_KEY_ID"),
            "scrubbed_inherit must NOT contain AWS_ACCESS_KEY_ID, got {env:?}"
        );
    }

    /// PATH must always be inherited so the spawned hook can resolve
    /// its command (a bare filename is intentionally passed to the OS
    /// PATH lookup — see [`crate::engine::HookEngine::spawn_one`]).
    #[test]
    fn path_is_inherited() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // PATH is virtually always set in any reasonable test
        // environment. If it's not (truly hermetic builds), skip
        // rather than spuriously fail.
        if std::env::var_os("PATH").is_none() {
            return;
        }
        let env = scrubbed_inherit();
        assert!(env.contains_key("PATH"), "PATH must be inherited");
    }

    #[test]
    fn anthropic_api_key_is_not_inherited() {
        let _lock = ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        let key = "ANTHROPIC_API_KEY";
        let previous = std::env::var(key).ok();
        std::env::set_var(key, "sk-ant-fake-leak-check");
        let _guard = EnvVarGuard { key, previous };

        let env = scrubbed_inherit();
        assert!(
            !env.contains_key("ANTHROPIC_API_KEY"),
            "scrubbed_inherit must NOT contain ANTHROPIC_API_KEY"
        );
    }
}
