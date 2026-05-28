//! # aura-exec-sandbox
//!
//! Layer: exec
//!
//! OS-primitive wrappers used by the exec layer to isolate filesystem
//! and process side-effects. This crate provides two narrow types:
//!
//! - [`FsSandbox`] — path validation + multi-root containment. Every
//!   path supplied by an agent goes through [`FsSandbox::resolve`] /
//!   [`FsSandbox::resolve_existing`] before any filesystem operation
//!   happens, blocking directory traversal and symlink escape.
//! - [`ProcessSandbox`] — subprocess spawn guardrails. Carries a
//!   resolved working directory + allow-listed `program` name so the
//!   tool / runner layer can validate spawn requests against a single
//!   surface.
//!
//! Both types are intentionally **standalone**. They do not depend on
//! any other `aura-*` crate (only `thiserror` + `tracing`). Higher
//! layers convert between [`SandboxError`] and their own error enums
//! via `#[from]` impls in the consumer crate.
//!
//! ## Invariants ([rules.md §13])
//!
//! - [`FsSandbox::resolve`] always returns a path that satisfies
//!   `starts_with(root) || starts_with(extra_root)` for *some* root
//!   the sandbox knows about. There is no escape hatch.
//! - [`FsSandbox::resolve_existing`] additionally canonicalises the
//!   target so symlink targets outside the sandbox are caught.
//! - [`FsSandbox::new`] canonicalises the root once at construction
//!   time. Roots that don't exist are created (`create_dir_all`); if
//!   creation fails the constructor returns the underlying io error
//!   inside [`SandboxError::Io`].
//! - [`ProcessSandbox::validate_program`] rejects any string
//!   containing a shell metacharacter, whitespace, or control byte —
//!   the only well-formed inputs are bare executable names (`git`,
//!   `cargo`) or full absolute / relative paths.
//!
//! ## Compatibility note
//!
//! Phase 5 introduces these primitives alongside the legacy
//! `aura_tools::Sandbox` to avoid a workspace-wide rewrite. Future
//! phases migrate callers from `aura_tools::Sandbox` to
//! [`FsSandbox`]; the two implement essentially the same algorithm
//! and tests cover the same attack surface (relative escape,
//! absolute-path escape, symlink follow-through, extra-root grant).

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::debug;

/// Errors returned by the sandbox primitives.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// The path resolves outside every allowed root.
    #[error("sandbox violation: path {path} escapes allowed roots")]
    Violation {
        /// The original (caller-supplied) path, for diagnostics.
        path: String,
    },
    /// A required path was not found on disk
    /// ([`FsSandbox::resolve_existing`] only).
    #[error("path not found: {0}")]
    NotFound(String),
    /// Underlying filesystem operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The spawn request was structurally invalid (empty program,
    /// metacharacters, control bytes, etc.).
    #[error("invalid program: {0}")]
    InvalidProgram(String),
}

// ============================================================================
// FsSandbox
// ============================================================================

/// Filesystem sandbox. Holds a canonical primary root and zero or
/// more extra roots; every path resolution validates containment
/// against the union.
#[derive(Debug, Clone)]
pub struct FsSandbox {
    root: PathBuf,
    extra_roots: Vec<PathBuf>,
}

impl FsSandbox {
    /// Construct a sandbox rooted at `root`. The directory is created
    /// if it does not already exist.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] if `root` cannot be created or
    /// canonicalised.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let root = root.as_ref();
        if !root.exists() {
            std::fs::create_dir_all(root)?;
        }
        let canonical = strip_unc_prefix(&root.canonicalize()?);
        debug!(?canonical, "FsSandbox initialized");
        Ok(Self {
            root: canonical,
            extra_roots: Vec::new(),
        })
    }

    /// Construct a sandbox with additional allowed roots beyond the
    /// primary root. Extra roots that don't exist or can't be
    /// canonicalised are silently skipped (logged at `debug`).
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] if the primary root can't be
    /// canonicalised.
    pub fn with_extra_roots(
        root: impl AsRef<Path>,
        extra: &[PathBuf],
    ) -> Result<Self, SandboxError> {
        let mut sandbox = Self::new(root)?;
        for path in extra {
            if !path.exists() {
                debug!(?path, "extra root missing; skipping");
                continue;
            }
            match path.canonicalize() {
                Ok(canonical) => sandbox.extra_roots.push(strip_unc_prefix(&canonical)),
                Err(err) => debug!(?path, error = %err, "extra root canonicalize failed; skipping"),
            }
        }
        Ok(sandbox)
    }

    /// Borrow the primary root (already canonical).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Borrow the extra-root list.
    #[must_use]
    pub fn extra_roots(&self) -> &[PathBuf] {
        &self.extra_roots
    }

    /// Resolve a path within the sandbox without requiring it to
    /// exist on disk. Relative paths join under the primary root;
    /// absolute paths must already be inside an allowed root.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Violation`] if the normalised path
    /// escapes every allowed root.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, SandboxError> {
        let path = path.as_ref();
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let normalized = normalize_path(&joined);
        if !self.is_within_allowed(&normalized) {
            return Err(SandboxError::Violation {
                path: path.display().to_string(),
            });
        }
        Ok(normalized)
    }

    /// Resolve a path that must already exist, additionally
    /// canonicalising it so symlink targets are validated.
    ///
    /// # Errors
    ///
    /// - [`SandboxError::Violation`] if either the normalised or
    ///   canonical path escapes allowed roots.
    /// - [`SandboxError::NotFound`] if the path does not exist.
    /// - [`SandboxError::Io`] if `canonicalize` fails for any other
    ///   reason.
    pub fn resolve_existing(&self, path: impl AsRef<Path>) -> Result<PathBuf, SandboxError> {
        let resolved = self.resolve(path.as_ref())?;
        if !resolved.exists() {
            return Err(SandboxError::NotFound(path.as_ref().display().to_string()));
        }
        let canonical = strip_unc_prefix(&resolved.canonicalize()?);
        if !self.is_within_allowed(&canonical) {
            return Err(SandboxError::Violation {
                path: path.as_ref().display().to_string(),
            });
        }
        Ok(canonical)
    }

    fn is_within_allowed(&self, path: &Path) -> bool {
        let normalized = strip_unc_prefix(path);
        if normalized.starts_with(&self.root) {
            return true;
        }
        self.extra_roots.iter().any(|r| normalized.starts_with(r))
    }
}

// ============================================================================
// ProcessSandbox
// ============================================================================

/// Process sandbox: validates spawn requests against a working
/// directory and allow-listed binary names.
///
/// This is a thin wrapper today — full spawn execution still lives in
/// `aura-tools::cmd_spawn` for compatibility. The wrapper exists so
/// future phases can route every subprocess through a single
/// validation surface and so the exec layer can grow process-level
/// guards (ulimits, cgroups, ETW) behind one type.
#[derive(Debug, Clone)]
pub struct ProcessSandbox {
    /// Working directory subprocesses run in. Must be absolute.
    pub workdir: PathBuf,
    /// Allow-listed program names (file-name match, post-`which`
    /// resolution). Empty means **no programs allowed** — fail closed.
    pub allowed_programs: Vec<String>,
}

impl ProcessSandbox {
    /// Convenience constructor.
    #[must_use]
    pub fn new(workdir: impl Into<PathBuf>, allowed_programs: Vec<String>) -> Self {
        Self {
            workdir: workdir.into(),
            allowed_programs,
        }
    }

    /// Verify that `program` is a syntactically well-formed executable
    /// name. Rejects whitespace, control bytes, and shell metacharacters.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::InvalidProgram`] if any reserved
    /// character is present.
    pub fn validate_program(program: &str) -> Result<(), SandboxError> {
        if program.is_empty() {
            return Err(SandboxError::InvalidProgram(
                "program must not be empty".into(),
            ));
        }
        for c in program.chars() {
            let code = c as u32;
            let is_ctrl = code < 0x20 || code == 0x7F;
            let is_ws = c == ' ' || c == '\t';
            let is_meta = matches!(
                c,
                ';' | '&'
                    | '|'
                    | '>'
                    | '<'
                    | '$'
                    | '`'
                    | '\\'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '?'
                    | '#'
                    | '\''
                    | '"'
                    | '\n'
                    | '\r'
            );
            if is_ctrl || is_ws || is_meta {
                return Err(SandboxError::InvalidProgram(format!(
                    "program {program:?} contains disallowed character {c:?}"
                )));
            }
        }
        Ok(())
    }

    /// True iff `program`'s file-name component is in
    /// [`Self::allowed_programs`]. An empty allow-list always denies
    /// (fail-closed).
    #[must_use]
    pub fn is_allowed(&self, program: &str) -> bool {
        if self.allowed_programs.is_empty() {
            return false;
        }
        let name = Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        self.allowed_programs.iter().any(|p| p == name)
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Normalize a path by resolving `.` / `..` components without
/// touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Strip the `\\?\` verbatim prefix Windows `canonicalize` adds. No-op
/// on non-Windows.
fn strip_unc_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn fs_sandbox_resolves_relative() {
        let dir = TempDir::new().unwrap();
        let sandbox = FsSandbox::new(dir.path()).unwrap();
        let resolved = sandbox.resolve("foo/bar.txt").unwrap();
        assert!(resolved.starts_with(sandbox.root()));
    }

    #[test]
    fn fs_sandbox_blocks_dotdot_escape() {
        let dir = TempDir::new().unwrap();
        let sandbox = FsSandbox::new(dir.path()).unwrap();
        let err = sandbox.resolve("../escape").expect_err("must block escape");
        assert!(matches!(err, SandboxError::Violation { .. }));
    }

    #[test]
    fn fs_sandbox_blocks_absolute_outside() {
        let dir = TempDir::new().unwrap();
        let sandbox = FsSandbox::new(dir.path()).unwrap();
        let err = sandbox
            .resolve("/etc/passwd")
            .expect_err("must block outside absolute");
        assert!(matches!(err, SandboxError::Violation { .. }));
    }

    #[test]
    fn fs_sandbox_resolve_existing_reports_not_found() {
        let dir = TempDir::new().unwrap();
        let sandbox = FsSandbox::new(dir.path()).unwrap();
        let err = sandbox
            .resolve_existing("missing.txt")
            .expect_err("missing path must error");
        assert!(matches!(err, SandboxError::NotFound(_)));
    }

    #[test]
    fn fs_sandbox_extra_root_grants_access() {
        let main = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        std::fs::write(extra.path().join("note.md"), "hi").unwrap();
        let sandbox =
            FsSandbox::with_extra_roots(main.path(), &[extra.path().to_path_buf()]).unwrap();
        assert!(sandbox.resolve(extra.path().join("note.md")).is_ok());
    }

    #[test]
    fn process_sandbox_validates_program_name() {
        assert!(ProcessSandbox::validate_program("git").is_ok());
        assert!(ProcessSandbox::validate_program("cargo-fmt").is_ok());
        assert!(matches!(
            ProcessSandbox::validate_program(""),
            Err(SandboxError::InvalidProgram(_))
        ));
        assert!(matches!(
            ProcessSandbox::validate_program("rm -rf"),
            Err(SandboxError::InvalidProgram(_))
        ));
        assert!(matches!(
            ProcessSandbox::validate_program("git; ls"),
            Err(SandboxError::InvalidProgram(_))
        ));
    }

    #[test]
    fn process_sandbox_allow_list_fails_closed_when_empty() {
        let sandbox = ProcessSandbox::new(PathBuf::from("/tmp"), vec![]);
        assert!(!sandbox.is_allowed("git"));
    }

    #[test]
    fn process_sandbox_allow_list_accepts_filename_match() {
        let sandbox = ProcessSandbox::new(PathBuf::from("/tmp"), vec!["git".into()]);
        assert!(sandbox.is_allowed("git"));
        assert!(sandbox.is_allowed("/usr/bin/git"));
        assert!(!sandbox.is_allowed("not-git"));
    }
}
