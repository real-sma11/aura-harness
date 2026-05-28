//! Manifest discovery under a plugin source directory.
//!
//! ## Precedence
//!
//! When resolving a plugin package root, the loader checks the
//! following relative paths in order and uses the first existing
//! file:
//!
//! 1. `.aura-plugin/manifest.toml` (Aura-native namespace)
//! 2. `.codex-plugin/manifest.toml` (Codex compat alias)
//! 3. `.claude-plugin/manifest.toml` (Claude / Codex compat alias)
//!
//! All three normalise into a single [`PluginManifest`] shape — the
//! cache always writes back as `.aura-plugin.toml` regardless of
//! which alias the source used. The originating alias is preserved
//! in [`DiscoveredManifest::alias`] for diagnostics.
//!
//! ## Multi-version tie-break
//!
//! When `discover_all` finds multiple manifests sharing the same
//! [`PluginId`], the resolver picks the active one via
//! [`pick_preferred`]: `active = true` wins outright; ties go to the
//! higher semver.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::ManifestError;
use crate::manifest::PluginManifest;

/// Which on-disk alias supplied a [`PluginManifest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestAlias {
    /// `.aura-plugin/manifest.toml` (canonical).
    Aura,
    /// `.codex-plugin/manifest.toml` (Codex compat).
    Codex,
    /// `.claude-plugin/manifest.toml` (Claude / Codex compat).
    Claude,
}

impl ManifestAlias {
    /// Directory name on disk for this alias.
    #[must_use]
    pub const fn dir_name(self) -> &'static str {
        match self {
            Self::Aura => ".aura-plugin",
            Self::Codex => ".codex-plugin",
            Self::Claude => ".claude-plugin",
        }
    }

    /// Relative file path under a plugin source root.
    #[must_use]
    pub fn relative_path(self) -> PathBuf {
        Path::new(self.dir_name()).join("manifest.toml")
    }

    /// Precedence order: aura > codex > claude.
    pub const ALL: [Self; 3] = [Self::Aura, Self::Codex, Self::Claude];
}

/// A successfully-discovered manifest plus the alias it came from.
#[derive(Clone, Debug)]
pub struct DiscoveredManifest {
    /// The alias dir the manifest was sourced from.
    pub alias: ManifestAlias,
    /// Absolute path to the manifest file.
    pub path: PathBuf,
    /// Parsed + validated manifest.
    pub manifest: PluginManifest,
}

/// Discover a single manifest under `source`. Returns `Ok(None)` if
/// no alias dir contains a manifest file.
///
/// # Errors
///
/// Returns [`ManifestError`] for I/O failures reading an existing
/// manifest file or TOML / schema validation failures.
pub fn discover_manifest(
    source: impl AsRef<Path>,
) -> Result<Option<DiscoveredManifest>, ManifestError> {
    let source = source.as_ref();
    for alias in ManifestAlias::ALL {
        let path = source.join(alias.relative_path());
        if path.is_file() {
            let body = fs::read_to_string(&path)?;
            let manifest = PluginManifest::from_toml_str(&body)?;
            return Ok(Some(DiscoveredManifest {
                alias,
                path,
                manifest,
            }));
        }
    }
    Ok(None)
}

/// Tie-break helper: from an iterator of discovered manifests
/// sharing the same plugin id, return the preferred one. `active`
/// wins outright; ties go to higher semver.
#[must_use]
pub fn pick_preferred(candidates: &[DiscoveredManifest]) -> Option<&DiscoveredManifest> {
    candidates.iter().max_by(|a, b| {
        let a_active = a.manifest.active;
        let b_active = b.manifest.active;
        a_active
            .cmp(&b_active)
            .then_with(|| a.manifest.version.cmp(&b.manifest.version))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, alias: ManifestAlias, body: &str) {
        let path = dir.join(alias.relative_path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }

    fn minimal(id: &str, version: &str, active: bool) -> String {
        format!(
            r#"
manifest_version = "v1"
id = "{id}"
version = "{version}"
active = {active}
"#
        )
    }

    #[test]
    fn discovers_aura_alias_first() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            ManifestAlias::Aura,
            &minimal("a", "1.0.0", false),
        );
        write_manifest(
            tmp.path(),
            ManifestAlias::Codex,
            &minimal("c", "0.1.0", false),
        );
        let found = discover_manifest(tmp.path()).unwrap().expect("must find");
        assert_eq!(found.alias, ManifestAlias::Aura);
        assert_eq!(found.manifest.id.as_str(), "a");
    }

    #[test]
    fn falls_back_to_codex_then_claude() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            ManifestAlias::Claude,
            &minimal("cl", "0.1.0", false),
        );
        let found = discover_manifest(tmp.path()).unwrap().expect("must find");
        assert_eq!(found.alias, ManifestAlias::Claude);

        let tmp2 = TempDir::new().unwrap();
        write_manifest(
            tmp2.path(),
            ManifestAlias::Codex,
            &minimal("co", "0.1.0", false),
        );
        write_manifest(
            tmp2.path(),
            ManifestAlias::Claude,
            &minimal("cl", "0.1.0", false),
        );
        let found2 = discover_manifest(tmp2.path()).unwrap().expect("must find");
        assert_eq!(found2.alias, ManifestAlias::Codex);
    }

    #[test]
    fn no_manifest_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert!(discover_manifest(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn pick_preferred_active_wins() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            ManifestAlias::Aura,
            &minimal("x", "1.0.0", true),
        );
        let active = discover_manifest(tmp.path()).unwrap().unwrap();

        let tmp2 = TempDir::new().unwrap();
        write_manifest(
            tmp2.path(),
            ManifestAlias::Aura,
            &minimal("x", "9.0.0", false),
        );
        let higher_inactive = discover_manifest(tmp2.path()).unwrap().unwrap();

        let pool = [higher_inactive, active];
        let pick = pick_preferred(&pool).unwrap();
        assert_eq!(pick.manifest.version.to_string(), "1.0.0");
        assert!(pick.manifest.active);
    }

    #[test]
    fn pick_preferred_semver_tiebreak_when_no_active() {
        let tmp_a = TempDir::new().unwrap();
        write_manifest(
            tmp_a.path(),
            ManifestAlias::Aura,
            &minimal("x", "1.0.0", false),
        );
        let lower = discover_manifest(tmp_a.path()).unwrap().unwrap();

        let tmp_b = TempDir::new().unwrap();
        write_manifest(
            tmp_b.path(),
            ManifestAlias::Aura,
            &minimal("x", "2.0.0", false),
        );
        let higher = discover_manifest(tmp_b.path()).unwrap().unwrap();

        let pool = [lower, higher];
        let pick = pick_preferred(&pool).unwrap();
        assert_eq!(pick.manifest.version.to_string(), "2.0.0");
    }
}
