//! Read-only git helpers.
//!
//! Phase 2 remediation (Invariant §1 — Sole External Gateway):
//! **mutating** git operations (`git add`, `git commit`, `git push`,
//! `git remote set-url`) now live exclusively in
//! [`aura_tools::git_tool`](../../../aura-tools/src/git_tool/mod.rs),
//! where they run inside the kernel-mediated tool pipeline. This module
//! retains only the read-only helpers used by the agent surface
//! (`is_git_repo`, `list_unpushed_commits`) — the former is a filesystem
//! check, the latter reads commit metadata via `git log` and is
//! explicitly listed as a declared exception in `docs/invariants.md`.
//!
//! If you find yourself adding a mutating helper here, route it through
//! `aura_tools::git_tool` instead.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::debug;

/// Timeout used for the read-only helpers below. Mutating operations
/// have their own timeout managed by the git tools in `aura-tools`.
const GIT_READ_TIMEOUT: Duration = Duration::from_secs(aura_config::GIT_READ_TIMEOUT_SECS);

/// Lightweight record of a single commit surfaced by
/// [`list_unpushed_commits`]. Kept stable so aura-os / dev-loop events
/// that emit `GitPushed { commits }` do not churn when the push helper
/// is moved to `aura-tools`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo {
    pub sha: String,
    pub message: String,
}

/// Quick workspace-level check: does the directory contain a `.git`
/// entry? This is a pure filesystem probe — no subprocess, no kernel
/// mediation required.
#[must_use]
pub fn is_git_repo(project_root: &str) -> bool {
    Path::new(project_root).join(".git").exists()
}

async fn git_output(cmd: &mut Command, label: &str) -> Result<std::process::Output, String> {
    tokio::time::timeout(GIT_READ_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "git {label}: timed out after {}s",
                GIT_READ_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("git {label}: {e}"))
}

/// List commits on `HEAD` that have not yet been pushed to
/// `<remote>/<branch>`.
///
/// This is a **read-only** inspection used by dev-loop and orbit
/// telemetry. It does not mutate any repository state. When the
/// remote ref is missing (e.g. first push) we return an empty vector
/// rather than propagating an error.
pub async fn list_unpushed_commits(
    project_root: &str,
    remote: &str,
    branch: &str,
) -> Vec<CommitInfo> {
    let range = format!("{remote}/{branch}..HEAD");
    let output = git_output(
        Command::new("git")
            .args(["log", &range, "--pretty=format:%H %s"])
            .current_dir(project_root),
        "log (unpushed)",
    )
    .await;

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|line| {
                let (sha, msg) = line.split_once(' ')?;
                Some(CommitInfo {
                    sha: sha.to_string(),
                    message: msg.to_string(),
                })
            })
            .collect(),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            debug!(%remote, %branch, %stderr, "no unpushed commits or ref not found");
            Vec::new()
        }
        Err(e) => {
            tracing::warn!(%remote, %branch, error = %e, "failed to list unpushed commits");
            Vec::new()
        }
    }
}
