//! # aura-exec-isolation
//!
//! Layer: exec
//!
//! Subagent execution isolation primitives. [`WorktreeIsolation`] is the
//! **primary** parallel-safety mechanism — when the operator's
//! workspace is a git repo, sibling agents run against a private `git
//! worktree add` checkout so their writes never race. [`CopyIsolation`]
//! is a fallback for when git is unavailable or the workspace is not a
//! repo; it cp-recursively snapshots the workspace into a private
//! directory.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Provision is **idempotent per `(source, isolation_id)`** —
//!   calling [`Isolation::provision`] twice with the same arguments
//!   returns the existing [`IsolatedWorkspace`] without re-running the
//!   provisioning subprocess. This lets restarts pick up half-finished
//!   isolations without crashing.
//! - Teardown is **best-effort**. A failed teardown never panics; the
//!   caller (typically Phase 7b's orphan reaper) will sweep stragglers
//!   under `$AURA_HOME/state/orphans/`. The trait still returns
//!   `Result<()>` so callers that *want* to surface the failure can.
//! - [`IsolatedWorkspace::root`] is always absolute. Provisioning
//!   verifies `source` is absolute (returns
//!   [`IsolationError::SourceNotAbsolute`] otherwise) and joins the
//!   `isolation_id` onto the absolute `worktree_root` /
//!   `workspace_root`.
//! - This crate has **no dependency on any other `aura-*` crate**. The
//!   intent is that the conflict + isolation primitives stay reusable
//!   from future Phase 7 isolation orchestrators without dragging the
//!   tool / runner layer along.
//!
//! ## Failure modes
//!
//! - [`IsolationError::Git`] — `git worktree add` / `git worktree remove`
//!   failed; stderr is propagated verbatim.
//! - [`IsolationError::Io`] — filesystem operation failed (copy fallback
//!   or worktree directory creation).
//! - [`IsolationError::SourceNotAbsolute`] — caller-supplied `source` was
//!   relative; we refuse to guess what to canonicalize against.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;
use tracing::{debug, warn};

/// Errors that can be returned by an [`Isolation`] implementation.
#[derive(Debug, Error)]
pub enum IsolationError {
    /// A `git` subprocess returned a non-zero exit status. The string
    /// contains the captured stderr (or a synthesized message if stderr
    /// was empty).
    #[error("git error: {0}")]
    Git(String),
    /// Underlying filesystem operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// `source` must be absolute so the resulting checkout path is
    /// well-defined.
    #[error("source workspace is not absolute: {0}")]
    SourceNotAbsolute(PathBuf),
}

/// Strategy used to provision an [`IsolatedWorkspace`]. Distinct from
/// the [`Isolation`] trait implementor: a future supervisor that wants
/// "worktree-then-copy-fallback" can read this discriminant on the
/// returned workspace to decide whether to retry on a different
/// volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IsolationStrategy {
    /// Provisioned via `git worktree add`.
    Worktree,
    /// Provisioned via a recursive directory copy.
    Copy,
}

/// A live isolated workspace ready to be used by a child agent.
///
/// Dropping this value does **not** automatically tear the workspace
/// down — call [`Isolation::teardown`] explicitly so failure modes
/// surface to the caller.
#[derive(Clone, Debug)]
pub struct IsolatedWorkspace {
    /// Absolute path to the isolated checkout / copy.
    pub root: PathBuf,
    /// How this workspace was provisioned.
    pub strategy: IsolationStrategy,
}

/// Provision and tear down isolated workspaces.
pub trait Isolation: Send + Sync {
    /// Provision an isolated workspace seeded from `source`.
    ///
    /// `isolation_id` is an opaque caller-supplied identifier
    /// (typically a child agent id or a deterministic hash). Calling
    /// twice with the same `isolation_id` reuses the existing
    /// workspace.
    ///
    /// # Errors
    ///
    /// See [`IsolationError`] for the failure variants.
    fn provision(
        &self,
        source: &Path,
        isolation_id: &str,
    ) -> Result<IsolatedWorkspace, IsolationError>;

    /// Tear down a previously provisioned [`IsolatedWorkspace`].
    ///
    /// Implementations are **best-effort**: a failed teardown never
    /// panics and may surface as a warn-level log. Phase 7b's orphan
    /// reaper sweeps any stragglers under
    /// `$AURA_HOME/state/orphans/`.
    ///
    /// # Errors
    ///
    /// See [`IsolationError`] for the failure variants.
    fn teardown(&self, workspace: &IsolatedWorkspace) -> Result<(), IsolationError>;
}

/// Worktree-backed isolation. `worktree_root` is the parent directory
/// under which each provisioned worktree lives at
/// `worktree_root/<isolation_id>`.
#[derive(Clone, Debug)]
pub struct WorktreeIsolation {
    /// Parent directory under which per-isolation worktrees are
    /// created.
    pub worktree_root: PathBuf,
}

impl WorktreeIsolation {
    /// Convenience constructor.
    #[must_use]
    pub fn new(worktree_root: impl Into<PathBuf>) -> Self {
        Self {
            worktree_root: worktree_root.into(),
        }
    }
}

impl Isolation for WorktreeIsolation {
    fn provision(
        &self,
        source: &Path,
        isolation_id: &str,
    ) -> Result<IsolatedWorkspace, IsolationError> {
        validate_absolute(source)?;
        std::fs::create_dir_all(&self.worktree_root)?;
        let target = self.worktree_root.join(isolation_id);
        if target.exists() {
            debug!(?target, "worktree already provisioned; reusing");
            return Ok(IsolatedWorkspace {
                root: target,
                strategy: IsolationStrategy::Worktree,
            });
        }
        // Windows: `canonicalize` returns `\\?\C:\...` UNC paths that
        // `git worktree add` rejects with "Invalid argument". Strip
        // the prefix so git sees a plain drive-letter path. On
        // non-Windows this is a no-op.
        let cwd = strip_unc_prefix(source);
        let target_arg = strip_unc_prefix(&target);
        let output = Command::new("git")
            .current_dir(&cwd)
            .args([
                "worktree",
                "add",
                target_arg.to_string_lossy().as_ref(),
                "HEAD",
            ])
            .output()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let detail = if stderr.trim().is_empty() {
                format!("git worktree add exited with status {}", output.status)
            } else {
                stderr
            };
            return Err(IsolationError::Git(detail));
        }
        Ok(IsolatedWorkspace {
            root: target,
            strategy: IsolationStrategy::Worktree,
        })
    }

    fn teardown(&self, workspace: &IsolatedWorkspace) -> Result<(), IsolationError> {
        let target_arg = strip_unc_prefix(&workspace.root);
        // `git worktree remove` needs to run inside the worktree
        // itself (or the main repo) so git can locate the worktree
        // registry. Running from the worktree is the simplest reliable
        // option; if it already disappeared, fall through to the
        // filesystem fallback.
        let cwd_for_git = if workspace.root.exists() {
            workspace.root.clone()
        } else {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        };
        let git_result = Command::new("git")
            .current_dir(strip_unc_prefix(&cwd_for_git))
            .args([
                "worktree",
                "remove",
                "-f",
                target_arg.to_string_lossy().as_ref(),
            ])
            .output();
        match git_result {
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                warn!(
                    target = ?workspace.root,
                    stderr = %stderr,
                    "git worktree remove failed; falling back to fs cleanup"
                );
            }
            Err(err) => warn!(
                target = ?workspace.root,
                error = %err,
                "git worktree remove failed to launch; falling back to fs cleanup"
            ),
            Ok(_) => {}
        }
        // Belt-and-suspenders cleanup: `git worktree remove` does not
        // always delete the on-disk directory on Windows (especially
        // when invoked outside the main repo). Per the
        // best-effort invariant in the module docs we sweep it
        // ourselves and downgrade failures to a warn.
        if workspace.root.exists() {
            if let Err(err) = std::fs::remove_dir_all(&workspace.root) {
                warn!(
                    target = ?workspace.root,
                    error = %err,
                    "fs teardown failed; leaving orphan for Phase 7b sweeper"
                );
            }
        }
        Ok(())
    }
}

/// Copy-backed isolation. `workspace_root` is the parent directory
/// under which each provisioned copy lives at
/// `workspace_root/<isolation_id>`.
#[derive(Clone, Debug)]
pub struct CopyIsolation {
    /// Parent directory under which per-isolation copies are created.
    pub workspace_root: PathBuf,
}

impl CopyIsolation {
    /// Convenience constructor.
    #[must_use]
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }
}

impl Isolation for CopyIsolation {
    fn provision(
        &self,
        source: &Path,
        isolation_id: &str,
    ) -> Result<IsolatedWorkspace, IsolationError> {
        validate_absolute(source)?;
        std::fs::create_dir_all(&self.workspace_root)?;
        let target = self.workspace_root.join(isolation_id);
        if target.exists() {
            debug!(?target, "copy isolation already provisioned; reusing");
            return Ok(IsolatedWorkspace {
                root: target,
                strategy: IsolationStrategy::Copy,
            });
        }
        copy_dir_recursive(source, &target)?;
        Ok(IsolatedWorkspace {
            root: target,
            strategy: IsolationStrategy::Copy,
        })
    }

    fn teardown(&self, workspace: &IsolatedWorkspace) -> Result<(), IsolationError> {
        if workspace.root.exists() {
            if let Err(err) = std::fs::remove_dir_all(&workspace.root) {
                warn!(
                    target = ?workspace.root,
                    error = %err,
                    "copy isolation teardown failed; leaving orphan for Phase 7b sweeper"
                );
            }
        }
        Ok(())
    }
}

fn validate_absolute(source: &Path) -> Result<(), IsolationError> {
    if source.is_absolute() {
        Ok(())
    } else {
        Err(IsolationError::SourceNotAbsolute(source.to_path_buf()))
    }
}

/// Strip the `\\?\` Windows verbatim prefix from a canonicalised
/// path. On non-Windows this is a no-op. Needed because `git worktree`
/// rejects UNC-prefixed Windows paths with "Invalid argument".
fn strip_unc_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// Recursive `cp -r` style copy. Symlinks are followed and their
/// targets copied as plain files / directories (matches the plan's
/// "symlinks → real files" rule).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // `metadata()` follows symlinks; `symlink_metadata()` does not.
        // We deliberately want the followed metadata so symlinks
        // resolve to their targets.
        let meta = std::fs::metadata(&from)?;
        if meta.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn provision_rejects_relative_source() {
        let tmp = TempDir::new().unwrap();
        let iso = CopyIsolation::new(tmp.path().to_path_buf());
        let err = iso
            .provision(Path::new("relative/path"), "id")
            .expect_err("relative source must be rejected");
        assert!(matches!(err, IsolationError::SourceNotAbsolute(_)));
    }

    #[test]
    fn copy_isolation_provision_then_teardown() {
        let src = TempDir::new().unwrap();
        let parent = TempDir::new().unwrap();
        std::fs::write(src.path().join("file.txt"), b"hello").unwrap();
        std::fs::create_dir(src.path().join("nested")).unwrap();
        std::fs::write(src.path().join("nested").join("inner.txt"), b"world").unwrap();

        let iso = CopyIsolation::new(parent.path().to_path_buf());
        let ws = iso
            .provision(src.path(), "child-1")
            .expect("provision must succeed");
        assert_eq!(ws.strategy, IsolationStrategy::Copy);
        assert!(ws.root.is_absolute());
        assert_eq!(
            std::fs::read(ws.root.join("file.txt")).unwrap(),
            b"hello".to_vec()
        );
        assert_eq!(
            std::fs::read(ws.root.join("nested").join("inner.txt")).unwrap(),
            b"world".to_vec()
        );

        iso.teardown(&ws).expect("teardown must succeed");
        assert!(!ws.root.exists());
    }

    #[test]
    fn copy_isolation_provision_is_idempotent() {
        let src = TempDir::new().unwrap();
        let parent = TempDir::new().unwrap();
        std::fs::write(src.path().join("a.txt"), b"a").unwrap();
        let iso = CopyIsolation::new(parent.path().to_path_buf());
        let ws1 = iso.provision(src.path(), "id").unwrap();
        let ws2 = iso.provision(src.path(), "id").unwrap();
        assert_eq!(ws1.root, ws2.root);
    }
}
