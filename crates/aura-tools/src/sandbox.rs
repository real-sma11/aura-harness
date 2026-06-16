//! Sandbox for path validation.
//!
//! Ensures all file-system paths supplied by an agent resolve within the
//! workspace root, preventing access to the wider host file system.
//!
//! # How Enforcement Works
//!
//! Every path goes through two stages of validation:
//!
//! 1. **Normalisation** – the path is joined with the sandbox root (if
//!    relative) and then [`normalize_path`] resolves all `.` and `..`
//!    components *without* touching the file system.  The result is compared
//!    against the canonical root via [`Path::starts_with`].
//! 2. **Symlink re-check** – for paths that must already exist
//!    ([`Sandbox::resolve_existing`]), the path is additionally
//!    [`canonicalize`](std::fs::canonicalize)d, which follows symlinks to their
//!    real target, and the prefix check is repeated.  This catches symlinks
//!    whose target lies outside the sandbox.
//!
//! # Attacks Prevented
//!
//! * **Directory traversal** (`../../../etc/passwd`) – caught during
//!   normalisation because `..` components that would move above the root are
//!   collapsed and the prefix check fails.
//! * **Symlinks / junctions to outside** – a symlink at
//!   `<root>/escape -> /etc` is caught by the post-canonicalize prefix check
//!   in `resolve_existing`.
//! * **Absolute paths outside root** (`/tmp/evil`) – fail the prefix check
//!   immediately.
//!
//! # Assumptions
//!
//! * **`workspace_root` is trusted** – the root path itself is provided by the
//!   system, not the agent.  It is canonicalized once at construction time.
//! * **No TOCTOU for new files** – [`Sandbox::resolve_new`] validates the
//!   *intended* path but cannot follow symlinks for files that do not yet
//!   exist.  A race where a symlink is created between validation and use is
//!   outside the sandbox's scope (mitigated at the OS/container level).
//! * **OS path semantics** – the normalisation logic relies on
//!   [`std::path::Component`] for correct handling of platform-specific path
//!   separators and prefixes (e.g. `\\?\` on Windows).

use crate::error::ToolError;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Sandbox for validating and normalizing paths.
#[derive(Debug, Clone)]
pub struct Sandbox {
    /// The primary root directory (workspace).
    root: PathBuf,
    /// Additional allowed roots granted by skill permissions.
    extra_roots: Vec<PathBuf>,
}

impl Sandbox {
    /// Create a new sandbox with the given root.
    ///
    /// # Errors
    /// Returns error if root cannot be canonicalized.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = root.as_ref();

        if !root.exists() {
            std::fs::create_dir_all(root).map_err(|e| {
                ToolError::Io(std::io::Error::new(
                    e.kind(),
                    format!("create_dir_all({}): {e}", root.display()),
                ))
            })?;
        }

        let root = strip_unc_prefix(&root.canonicalize().map_err(|e| {
            ToolError::Io(std::io::Error::new(
                e.kind(),
                format!("canonicalize({}): {e}", root.display()),
            ))
        })?);
        debug!(?root, "Sandbox initialized");

        Ok(Self {
            root,
            extra_roots: Vec::new(),
        })
    }

    /// Create a sandbox with extra allowed roots (from skill permissions).
    ///
    /// Each extra root is canonicalized. Roots that don't exist or can't
    /// be canonicalized are silently skipped (logged as warnings).
    ///
    /// # Errors
    /// Returns error if the primary root cannot be canonicalized.
    pub fn with_extra_roots(root: impl AsRef<Path>, extra: &[PathBuf]) -> Result<Self, ToolError> {
        let mut sandbox = Self::new(root)?;
        for path in extra {
            if !path.exists() {
                debug!(?path, "Extra sandbox root does not exist, skipping");
                continue;
            }
            match path.canonicalize() {
                Ok(canonical) => {
                    let canonical = strip_unc_prefix(&canonical);
                    debug!(?canonical, "Added extra sandbox root");
                    sandbox.extra_roots.push(canonical);
                }
                Err(e) => {
                    debug!(?path, error = %e, "Failed to canonicalize extra root, skipping");
                }
            }
        }
        Ok(sandbox)
    }

    /// Get the sandbox root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve and validate a path within the sandbox.
    ///
    /// The path can be:
    /// - Absolute (must be under root)
    /// - Relative (resolved relative to root)
    ///
    /// # Errors
    /// Returns `SandboxViolation` if the resolved path escapes the root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        let path = path.as_ref();

        if is_workspace_root_alias(path) {
            debug!(original = ?path, resolved = ?self.root, "Path resolved to workspace root alias");
            return Ok(self.root.clone());
        }

        // Join with root if relative
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };

        // Normalize the path (handle .., ., etc.)
        let normalized = normalize_path(&joined);

        if !self.is_within_allowed(&normalized) {
            return Err(ToolError::SandboxViolation {
                path: path.display().to_string(),
            });
        }

        debug!(original = ?path, resolved = ?normalized, "Path resolved");
        Ok(normalized)
    }

    /// Resolve a path that must exist.
    ///
    /// # Errors
    /// Returns error if path doesn't exist or escapes sandbox.
    pub fn resolve_existing(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        let resolved = match self.resolve(path.as_ref()) {
            Ok(resolved) => resolved,
            Err(err @ ToolError::SandboxViolation { .. }) => {
                let candidate = if path.as_ref().is_absolute() {
                    path.as_ref().to_path_buf()
                } else {
                    self.root.join(path.as_ref())
                };

                if !candidate.exists() {
                    return Err(err);
                }

                let canonical = strip_unc_prefix(&candidate.canonicalize().map_err(|e| {
                    ToolError::Io(std::io::Error::new(
                        e.kind(),
                        format!("canonicalize({}): {e}", candidate.display()),
                    ))
                })?);

                if self.is_within_allowed(&canonical) {
                    return Ok(canonical);
                }

                return Err(err);
            }
            Err(err) => return Err(err),
        };

        if !resolved.exists() {
            return Err(ToolError::PathNotFound(path.as_ref().display().to_string()));
        }

        let canonical = strip_unc_prefix(&resolved.canonicalize().map_err(|e| {
            ToolError::Io(std::io::Error::new(
                e.kind(),
                format!("canonicalize({}): {e}", resolved.display()),
            ))
        })?);

        if !self.is_within_allowed(&canonical) {
            return Err(ToolError::SandboxViolation {
                path: path.as_ref().display().to_string(),
            });
        }

        Ok(canonical)
    }

    /// Check whether a path falls under the primary root or any extra root.
    fn is_within_allowed(&self, path: &Path) -> bool {
        let normalized = strip_unc_prefix(path);
        if normalized.starts_with(&self.root) {
            return true;
        }
        self.extra_roots.iter().any(|r| normalized.starts_with(r))
    }

    /// Resolve a path for a new file (doesn't need to exist).
    ///
    /// This validates that the target path would be within the sandbox
    /// but doesn't require the file to already exist.
    ///
    /// # Errors
    /// Returns error if path would escape sandbox.
    pub fn resolve_new(&self, path: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
        self.resolve(path)
    }
}

/// Normalize a path by resolving `.` and `..` components.
///
/// Unlike `canonicalize`, this doesn't require the path to exist.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                // Go up one level if possible
                if !components.is_empty() {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {
                // Skip current dir references
            }
            other => {
                components.push(other);
            }
        }
    }

    components.iter().collect()
}

/// Strip the `\\?\` verbatim prefix that Windows `canonicalize()` adds.
/// On non-Windows this is a no-op.
fn strip_unc_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

fn is_workspace_root_alias(path: &Path) -> bool {
    use std::path::Component;

    let mut components = path.components();
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(Component::RootDir), None, None)
            | (Some(Component::Prefix(_)), Some(Component::RootDir), None)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_sandbox() -> (Sandbox, TempDir) {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        (sandbox, dir)
    }

    #[test]
    fn test_resolve_relative() {
        let (sandbox, _dir) = create_sandbox();

        let resolved = sandbox.resolve("foo/bar.txt").unwrap();
        assert!(resolved.starts_with(sandbox.root()));
        assert!(resolved.ends_with("foo/bar.txt"));
    }

    #[test]
    fn test_resolve_absolute_inside() {
        let (sandbox, _dir) = create_sandbox();

        let path = sandbox.root().join("foo/bar.txt");
        let resolved = sandbox.resolve(&path).unwrap();
        assert_eq!(resolved, path);
    }

    #[test]
    fn test_resolve_dotdot_escape() {
        let (sandbox, _dir) = create_sandbox();

        let result = sandbox.resolve("../escape.txt");
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn test_resolve_complex_dotdot() {
        let (sandbox, _dir) = create_sandbox();

        // foo/../bar should be fine (stays in root)
        let resolved = sandbox.resolve("foo/../bar.txt").unwrap();
        assert!(resolved.starts_with(sandbox.root()));

        // foo/../../escape should fail
        let result = sandbox.resolve("foo/../../escape.txt");
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn test_resolve_absolute_outside() {
        let (sandbox, _dir) = create_sandbox();

        let result = sandbox.resolve("/etc/passwd");
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn test_resolve_workspace_root_alias() {
        let (sandbox, _dir) = create_sandbox();

        let resolved = sandbox.resolve("/").unwrap();
        assert_eq!(resolved, sandbox.root());
    }

    #[test]
    fn test_resolve_existing() {
        let (sandbox, dir) = create_sandbox();

        // Create a file
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let resolved = sandbox.resolve_existing("test.txt").unwrap();
        let expected = strip_unc_prefix(&file_path.canonicalize().unwrap());
        assert_eq!(resolved, expected);
    }

    #[test]
    fn test_resolve_existing_not_found() {
        let (sandbox, _dir) = create_sandbox();

        let result = sandbox.resolve_existing("nonexistent.txt");
        assert!(matches!(result, Err(ToolError::PathNotFound(_))));
    }

    #[cfg(unix)]
    #[test]
    fn test_symlink_pointing_outside_sandbox_blocked() {
        use std::os::unix::fs::symlink;

        let (sandbox, dir) = create_sandbox();

        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        symlink(
            outside.path().join("secret.txt"),
            dir.path().join("escape_link"),
        )
        .unwrap();

        let result = sandbox.resolve_existing("escape_link");
        assert!(
            matches!(result, Err(ToolError::SandboxViolation { .. })),
            "Symlink to outside should be blocked, got: {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_symlink_directory_junction_outside_blocked() {
        // On Windows, directory junctions don't require elevated privileges
        let (sandbox, dir) = create_sandbox();

        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();

        // Create a junction point (requires std::process::Command)
        let junction_path = dir.path().join("escape_junction");
        let status = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction_path.to_string_lossy(),
                &outside.path().to_string_lossy(),
            ])
            .output();

        if let Ok(output) = status {
            if output.status.success() {
                let result = sandbox.resolve_existing("escape_junction/secret.txt");
                assert!(
                    matches!(result, Err(ToolError::SandboxViolation { .. })),
                    "Junction to outside should be blocked, got: {result:?}"
                );
            }
            // If mklink fails (e.g. permissions), skip the test gracefully
        }
    }

    #[test]
    fn test_resolve_new_allows_nonexistent_path() {
        let (sandbox, _dir) = create_sandbox();

        let result = sandbox.resolve_new("brand/new/file.txt");
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert!(resolved.starts_with(sandbox.root()));
    }

    #[test]
    fn test_resolve_new_blocks_escape() {
        let (sandbox, _dir) = create_sandbox();

        let result = sandbox.resolve_new("../../etc/passwd");
        assert!(matches!(result, Err(ToolError::SandboxViolation { .. })));
    }

    #[test]
    fn test_sandbox_root_is_canonical() {
        let dir = TempDir::new().unwrap();
        let sandbox = Sandbox::new(dir.path()).unwrap();
        let root = sandbox.root();
        // Canonical path should not contain "." or ".."
        for component in root.components() {
            assert_ne!(component, std::path::Component::CurDir);
            assert_ne!(component, std::path::Component::ParentDir);
        }
    }

    #[test]
    fn test_sandbox_clone() {
        let (sandbox, _dir) = create_sandbox();
        let cloned = sandbox.clone();
        assert_eq!(sandbox.root(), cloned.root());
    }

    #[test]
    fn test_extra_roots_allow_access() {
        let main_dir = TempDir::new().unwrap();
        let extra_dir = TempDir::new().unwrap();
        std::fs::write(extra_dir.path().join("note.md"), "hello").unwrap();

        let sandbox =
            Sandbox::with_extra_roots(main_dir.path(), &[extra_dir.path().to_path_buf()]).unwrap();

        let resolved = sandbox.resolve_existing(extra_dir.path().join("note.md"));
        assert!(
            resolved.is_ok(),
            "should be able to access file in extra root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_extra_roots_allow_access_through_canonical_alias() {
        use std::os::unix::fs::symlink;

        let main_dir = TempDir::new().unwrap();
        let extra_dir = TempDir::new().unwrap();
        let alias_dir = TempDir::new().unwrap();
        std::fs::write(extra_dir.path().join("note.md"), "hello").unwrap();
        let alias = alias_dir.path().join("extra");
        symlink(extra_dir.path(), &alias).unwrap();

        let sandbox =
            Sandbox::with_extra_roots(main_dir.path(), &[extra_dir.path().to_path_buf()]).unwrap();

        let resolved = sandbox.resolve_existing(alias.join("note.md"));
        assert!(
            resolved.is_ok(),
            "should accept an existing extra-root path whose spelling canonicalizes inside the extra root"
        );
    }

    #[test]
    fn test_extra_roots_still_block_outside() {
        let main_dir = TempDir::new().unwrap();
        let extra_dir = TempDir::new().unwrap();
        let outside_dir = TempDir::new().unwrap();
        std::fs::write(outside_dir.path().join("secret.txt"), "secret").unwrap();

        let sandbox =
            Sandbox::with_extra_roots(main_dir.path(), &[extra_dir.path().to_path_buf()]).unwrap();

        let result = sandbox.resolve(outside_dir.path().join("secret.txt"));
        assert!(
            matches!(result, Err(ToolError::SandboxViolation { .. })),
            "paths outside both roots should still be blocked"
        );
    }

    #[test]
    fn test_extra_roots_nonexistent_skipped() {
        let main_dir = TempDir::new().unwrap();
        let sandbox = Sandbox::with_extra_roots(
            main_dir.path(),
            &[PathBuf::from("/nonexistent/path/that/does/not/exist")],
        )
        .unwrap();

        assert_eq!(sandbox.extra_roots.len(), 0);
    }
}
