//! Error types for plugin manifest parse + install + cache pipeline.
//!
//! All errors here are `thiserror` enums per [rules.md §4]. The
//! `aura plugins` CLI handlers (in the root `aura` bin) wrap these
//! into `anyhow::Error` chains at the application boundary.

use std::path::PathBuf;

use thiserror::Error;

/// Reasons manifest parse / validation can fail.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// I/O error reading or writing a manifest file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// TOML parse error.
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),
    /// Manifest declares a schema version this build does not
    /// support. The integer carried is the wire-format version
    /// number (e.g. `99` for `manifest_version = "v99"` — but in
    /// practice unknown string variants surface via [`Self::Toml`];
    /// this variant remains for future numeric schemas).
    #[error("unsupported manifest version: {0}")]
    UnsupportedVersion(u8),
    /// Manifest parsed but failed semantic validation.
    #[error("invalid manifest schema: {0}")]
    InvalidSchema(String),
}

/// Reasons the install pipeline can fail.
#[derive(Debug, Error)]
pub enum PluginInstallError {
    /// Manifest parse or validation failed.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// I/O error during source enumeration or cache write.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Same `id@version` already installed and active in the cache.
    /// The caller may decide to reinstall by removing the existing
    /// dir first.
    #[error("plugin id `{id}` already installed at version {existing}")]
    AlreadyInstalled {
        /// Plugin id reported as already installed.
        id: String,
        /// Existing on-disk version.
        existing: String,
    },
    /// Source path passed to [`crate::install`] does not exist or is
    /// not a directory.
    #[error("source path is not a directory: {0}")]
    SourceNotDirectory(PathBuf),
    /// No manifest was found under any of the three discovery dirs
    /// (`.aura-plugin/`, `.codex-plugin/`, `.claude-plugin/`).
    #[error(
        "missing manifest file (looked for .aura-plugin/, .codex-plugin/, .claude-plugin/ under {0})"
    )]
    MissingManifest(PathBuf),
    /// Manifest declares `trust.require_explicit_trust = true` and
    /// the caller did not pass `trust_override = true`. The CLI
    /// surfaces this as a prompt; CI / scripted installs pass
    /// `--trust` to bypass.
    #[error("trust required: plugin `{0}` declares require_explicit_trust=true")]
    TrustRequired(String),
}
