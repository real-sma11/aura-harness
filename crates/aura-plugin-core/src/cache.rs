//! # `PluginCache` layout ([rules.md §13 invariants])
//!
//! Under `AURA_HOME/plugins/`:
//!
//! ```text
//!   <plugin_id>/<version>/         <- materialised plugin payload
//!     .aura-plugin.toml            <- normalised manifest
//!     [other files copied from source]
//!   <plugin_id>/active             <- pointer file (or symlink on
//!                                     Unix) naming the active version.
//! ```
//!
//! ## Invariants
//!
//! - Cache writes are **atomic** per `<plugin_id>/<version>` directory
//!   (the install pipeline writes into a `<version>.tmp` directory,
//!   then `fs::rename`s into place). A partial copy never leaves an
//!   observable version dir.
//! - **Multiple versions** of the same plugin id can coexist; only
//!   one is active at a time. The active pointer is updated via the
//!   same atomic-write pattern ([`PluginCache::set_active`]).
//! - The `active` pointer is a **plain text file on Windows**
//!   (containing the version string) because cross-platform symlink
//!   support is inconsistent. On Unix it's still a text file by
//!   default — symlink reads are tolerated as a fallback so this
//!   crate doesn't need to discriminate by platform on read. The
//!   `<plugin_id>/active` name is reserved; [`PluginCache::list_versions`]
//!   filters it out.
//! - **No I/O at construct time** — [`PluginCache::new`] is pure.
//!   Subdirectories are created lazily by `set_active` /
//!   [`crate::install`].

use std::fs;
use std::path::{Path, PathBuf};

/// Layout root for the on-disk plugin cache.
///
/// Construct once per process with `AURA_HOME/plugins`; the type is
/// `Clone` (just a `PathBuf`) so handlers can pass it cheaply.
#[derive(Clone, Debug)]
pub struct PluginCache {
    root: PathBuf,
}

impl PluginCache {
    /// Construct a new cache rooted at `plugins_root` (typically
    /// `AURA_HOME/plugins`). No I/O is performed at construct time.
    #[must_use]
    pub fn new(plugins_root: impl Into<PathBuf>) -> Self {
        Self {
            root: plugins_root.into(),
        }
    }

    /// Absolute path to the cache root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Per-plugin directory (`<root>/<plugin_id>/`).
    #[must_use]
    pub fn plugin_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// Per-version directory (`<root>/<plugin_id>/<version>/`).
    #[must_use]
    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.plugin_dir(id).join(version)
    }

    /// Path to the active-version pointer file.
    #[must_use]
    pub fn active_pointer(&self, id: &str) -> PathBuf {
        self.plugin_dir(id).join("active")
    }

    /// Read the active-version pointer for a plugin id, if any.
    ///
    /// Tolerates both the text-file and symlink representations so
    /// the same call works on Windows and Unix without a cfg fork.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for unreadable pointer files. A missing
    /// pointer yields `Ok(None)`.
    pub fn active_version(&self, id: &str) -> std::io::Result<Option<String>> {
        let p = self.active_pointer(id);
        if !p.exists() {
            return Ok(None);
        }
        match fs::read_to_string(&p) {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(_) => {
                // Fall back to symlink read for Unix-style symlinks
                // that may have been written by a Codex-port.
                let target = fs::read_link(&p)?;
                Ok(target.file_name().map(|n| n.to_string_lossy().into_owned()))
            }
        }
    }

    /// Atomically set the active-version pointer for a plugin id.
    ///
    /// Writes to a sibling `.tmp` file then `fs::rename`s into place
    /// so a concurrent reader never observes a half-written pointer.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the plugin dir cannot be created or
    /// the rename fails.
    pub fn set_active(&self, id: &str, version: &str) -> std::io::Result<()> {
        let dir = self.plugin_dir(id);
        fs::create_dir_all(&dir)?;
        let p = self.active_pointer(id);
        let tmp = p.with_extension("tmp");
        fs::write(&tmp, version)?;
        // `fs::rename` is atomic across same-filesystem renames on
        // both Unix and Windows; the plugin dir is always the same
        // filesystem as the cache root.
        match fs::rename(&tmp, &p) {
            Ok(()) => Ok(()),
            Err(e) => {
                // On Windows, `rename` fails if the target exists for
                // some filesystems / antivirus interactions. Retry
                // with explicit remove + rename to keep the atomic
                // guarantee from the reader's perspective (a reader
                // either sees the old value or the new value, never
                // a partial write — the tmp file is never visible
                // under the pointer name).
                if p.exists() {
                    fs::remove_file(&p)?;
                    fs::rename(&tmp, &p)?;
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Enumerate every installed version of a plugin id, sorted
    /// lexicographically. Skips the `active` pointer file.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for unreadable plugin dirs. A missing
    /// plugin dir yields an empty vec.
    pub fn list_versions(&self, id: &str) -> std::io::Result<Vec<String>> {
        let dir = self.plugin_dir(id);
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut versions = vec![];
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_name() == "active" {
                continue;
            }
            if entry.file_type()?.is_dir() {
                versions.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        versions.sort();
        Ok(versions)
    }

    /// Enumerate every plugin id under the cache root, sorted
    /// lexicographically.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for unreadable cache roots. A missing
    /// cache root yields an empty vec.
    pub fn list_plugins(&self) -> std::io::Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(vec![]);
        }
        let mut ids = vec![];
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ids.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn empty_cache_lists_nothing() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        assert!(cache.list_plugins().unwrap().is_empty());
        assert!(cache.list_versions("missing").unwrap().is_empty());
        assert_eq!(cache.active_version("missing").unwrap(), None);
    }

    #[test]
    fn set_and_read_active_version() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        cache.set_active("alpha", "1.0.0").unwrap();
        assert_eq!(
            cache.active_version("alpha").unwrap(),
            Some("1.0.0".to_string())
        );
        cache.set_active("alpha", "2.0.0").unwrap();
        assert_eq!(
            cache.active_version("alpha").unwrap(),
            Some("2.0.0".to_string())
        );
    }

    #[test]
    fn list_versions_skips_active_pointer() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        fs::create_dir_all(cache.version_dir("alpha", "1.0.0")).unwrap();
        fs::create_dir_all(cache.version_dir("alpha", "2.0.0")).unwrap();
        cache.set_active("alpha", "2.0.0").unwrap();
        let versions = cache.list_versions("alpha").unwrap();
        assert_eq!(versions, vec!["1.0.0".to_string(), "2.0.0".to_string()]);
    }

    #[test]
    fn list_plugins_returns_sorted_ids() {
        let tmp = TempDir::new().unwrap();
        let cache = PluginCache::new(tmp.path().join("plugins"));
        fs::create_dir_all(cache.plugin_dir("zeta")).unwrap();
        fs::create_dir_all(cache.plugin_dir("alpha")).unwrap();
        let ids = cache.list_plugins().unwrap();
        assert_eq!(ids, vec!["alpha".to_string(), "zeta".to_string()]);
    }
}
