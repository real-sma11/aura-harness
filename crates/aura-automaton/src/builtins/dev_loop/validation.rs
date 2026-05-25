//! Post-execution validation: today this is just a build-preflight
//! gate. The historical `NeedsDecomposition` / decomposition-hint
//! plumbing was removed when the dev-loop stopped trying to police
//! the agent's tool-call shape; `validate_execution` is a thin
//! identity helper that the orchestrator still calls so future
//! verdicts have a clear hook.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aura_agent::agent_runner::TaskExecutionResult;

use crate::error::AutomatonError;

/// Hard timeout for the in-process build preflight. Mirrors the
/// server-side gate in `aura-os-server::handlers::dev_loop::signals::
/// build_preflight` so the two gates stay in lockstep.
const BUILD_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(90);

/// Outcome of [`validate_build_preflight`]. Returned by the helper so
/// the caller can decide whether to keep the task `Done` or demote it
/// to `Failed` via the existing failure path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPreflightOutcome {
    /// `true` when `cargo check` exited 0 within the timeout (or when
    /// the workspace isn't a Cargo project — the gate is Rust-only).
    pub ok: bool,
    /// First `Eddd` code surfaced by cargo, when extractable.
    pub first_error_code: Option<String>,
    /// Truncated tail of combined stdout+stderr (max 4 KiB).
    pub stderr_tail: String,
    /// `true` when the process was killed by the timeout.
    pub timed_out: bool,
}

/// True when the env var `AURA_BUILD_GATE` is set to a truthy value.
/// Orchestrators call this before invoking [`validate_build_preflight`]
/// so the gate is opt-in.
#[must_use]
pub fn build_preflight_gate_enabled() -> bool {
    std::env::var("AURA_BUILD_GATE").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

/// Run `cargo check --message-format=short --quiet` against the given
/// workspace. The dev-loop orchestrator calls this AFTER
/// [`validate_execution`] returns `Ok(_)` and BEFORE persisting the
/// task as `Done`; when the verdict is `ok == false` the orchestrator
/// demotes the task via the existing `AutomatonError::AgentExecution`
/// path so retry budgets / failure reasons keep working unchanged.
///
/// Returns `BuildPreflightOutcome { ok: true, .. }` when the
/// workspace isn't a Cargo project — non-Rust workspaces aren't
/// gated.
#[must_use]
pub fn validate_build_preflight(workspace_path: &str) -> BuildPreflightOutcome {
    if workspace_path.trim().is_empty() {
        return BuildPreflightOutcome {
            ok: false,
            first_error_code: None,
            stderr_tail: "build preflight: workspace path is empty".into(),
            timed_out: false,
        };
    }
    let path = Path::new(workspace_path);
    if !path.exists() {
        return BuildPreflightOutcome {
            ok: false,
            first_error_code: None,
            stderr_tail: format!(
                "build preflight: workspace does not exist on disk: {workspace_path}"
            ),
            timed_out: false,
        };
    }
    if !path.join("Cargo.toml").exists() && !path.join("Cargo.lock").exists() {
        // Not a Cargo workspace — skipped as a true verdict.
        return BuildPreflightOutcome {
            ok: true,
            first_error_code: None,
            stderr_tail: "build preflight: not a Cargo workspace (skipped)".into(),
            timed_out: false,
        };
    }

    let start = Instant::now();
    let child = Command::new("cargo")
        .args(["check", "--message-format=short", "--quiet"])
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .env("NO_COLOR", "1")
        .current_dir(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        return BuildPreflightOutcome {
            ok: false,
            first_error_code: None,
            stderr_tail:
                "build preflight: failed to spawn `cargo check` (cargo not on PATH?). \
                 Disable AURA_BUILD_GATE to silence."
                    .into(),
            timed_out: false,
        };
    };

    use std::io::Read;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = drain(&mut child.stdout.take());
                let stderr = drain(&mut child.stderr.take());
                let combined = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
                return BuildPreflightOutcome {
                    ok: status.success(),
                    first_error_code: first_error_code(&combined),
                    stderr_tail: truncate_tail(&combined, 4_000),
                    timed_out: false,
                };
            }
            Ok(None) => {
                if start.elapsed() >= BUILD_PREFLIGHT_TIMEOUT {
                    let _ = child.kill();
                    return BuildPreflightOutcome {
                        ok: false,
                        first_error_code: None,
                        stderr_tail: format!(
                            "build preflight: `cargo check` exceeded {}s timeout and was killed",
                            BUILD_PREFLIGHT_TIMEOUT.as_secs()
                        ),
                        timed_out: true,
                    };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                return BuildPreflightOutcome {
                    ok: false,
                    first_error_code: None,
                    stderr_tail: format!("build preflight: try_wait failed: {err}"),
                    timed_out: false,
                };
            }
        }
    }

    fn drain(handle: &mut Option<impl Read>) -> Vec<u8> {
        let mut buf = Vec::new();
        if let Some(h) = handle {
            let _ = h.read_to_end(&mut buf);
        }
        buf
    }
}

/// Convert a failing [`BuildPreflightOutcome`] into the matching
/// `AutomatonError::AgentExecution` so the orchestrator can plug the
/// verdict straight into the existing failure-handling path without
/// inventing a new variant. The returned message starts with the same
/// `build_preflight_failed:` discriminator the server-side gate uses
/// so dashboards / classifiers can recognise both sources uniformly.
#[must_use]
pub fn build_preflight_failure_to_error(outcome: &BuildPreflightOutcome) -> AutomatonError {
    let code = outcome
        .first_error_code
        .as_deref()
        .map_or_else(|| "unknown".to_string(), |c| format!("error[{c}]"));
    let message = if outcome.timed_out {
        "build_preflight_failed: `cargo check` exceeded the 90s timeout; \
         demoted task verdict to failure"
            .to_string()
    } else {
        format!(
            "build_preflight_failed: {code} surfaced by `cargo check`; \
             demoted task verdict to failure"
        )
    };
    AutomatonError::AgentExecution(message)
}

fn first_error_code(combined: &str) -> Option<String> {
    for line in combined.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("error[") {
            if let Some(end) = rest.find(']') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn truncate_tail(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let mut start = s.len().saturating_sub(limit);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("... (truncated to last {limit} bytes)\n{}", &s[start..])
}

/// Validate an agent-task execution result. The post-hoc "no file
/// ops" gate was removed (it was a behavioural valve, not a
/// correctness check), so this is now an identity pass that returns
/// the execution result unchanged. Kept as a function so the
/// orchestrator's call-site stays stable if a future verdict needs
/// to be wired back in.
pub(crate) fn validate_execution(
    exec: TaskExecutionResult,
) -> Result<TaskExecutionResult, AutomatonError> {
    Ok(exec)
}

#[cfg(test)]
mod build_preflight_tests {
    use super::*;

    #[test]
    fn build_preflight_gate_enabled_honours_env_var() {
        let key = "AURA_BUILD_GATE";
        let original = std::env::var(key).ok();
        std::env::set_var(key, "true");
        assert!(build_preflight_gate_enabled());
        std::env::set_var(key, "0");
        assert!(!build_preflight_gate_enabled());
        std::env::remove_var(key);
        assert!(!build_preflight_gate_enabled());
        if let Some(value) = original {
            std::env::set_var(key, value);
        }
    }

    #[test]
    fn validate_build_preflight_skips_non_cargo_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = validate_build_preflight(tmp.path().to_str().unwrap());
        assert!(outcome.ok, "non-cargo workspace must short-circuit ok");
        assert!(outcome.stderr_tail.contains("not a Cargo workspace"));
    }

    #[test]
    fn validate_build_preflight_rejects_missing_workspace() {
        let outcome = validate_build_preflight("");
        assert!(!outcome.ok);
        assert!(outcome.stderr_tail.contains("workspace path is empty"));
    }

    #[test]
    fn build_preflight_failure_to_error_carries_discriminator() {
        let outcome = BuildPreflightOutcome {
            ok: false,
            first_error_code: Some("E0432".to_string()),
            stderr_tail: String::new(),
            timed_out: false,
        };
        let AutomatonError::AgentExecution(msg) = build_preflight_failure_to_error(&outcome)
        else {
            panic!("expected AgentExecution");
        };
        assert!(msg.starts_with("build_preflight_failed:"));
        assert!(msg.contains("error[E0432]"));
    }

    #[test]
    fn build_preflight_failure_to_error_handles_timeout() {
        let outcome = BuildPreflightOutcome {
            ok: false,
            first_error_code: None,
            stderr_tail: String::new(),
            timed_out: true,
        };
        let AutomatonError::AgentExecution(msg) = build_preflight_failure_to_error(&outcome)
        else {
            panic!("expected AgentExecution");
        };
        assert!(msg.contains("exceeded the 90s timeout"));
    }

    #[test]
    fn first_error_code_extracts_e_designator() {
        let combined = "warning: x\nerror[E0277]: trait bound\n";
        assert_eq!(first_error_code(combined).as_deref(), Some("E0277"));
        assert_eq!(first_error_code("clean"), None);
    }
}
