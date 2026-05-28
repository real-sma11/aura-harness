//! Orphan handoff — persistence of detached / abandoned subagents
//! whose parent has exited.
//!
//! When a detached child outlives its parent (parent panic, natural
//! completion, or `JoinPolicy::Abandon`) the fleet writes a small
//! JSON record under [`OrphanStore::root`] / `<agent_id>.json` so the
//! `aura agents inspect/reap` commands can list and clean up the
//! orphan after process restart.
//!
//! # Schema (stable, JSON)
//!
//! ```json
//! {
//!   "agent_id": "<hex>",
//!   "parent_lineage": ["<hex>", "<hex>"],
//!   "mode": "agent",
//!   "kernel_mode": "audited_lite",
//!   "spawn_mode": "detached",
//!   "spawned_at": "2026-05-27T23:00:00Z",
//!   "kind": "<subagent_type>",
//!   "model_id": "claude-opus-4-7",
//!   "originating_user_id": "user-root"
//! }
//! ```
//!
//! # Invariants
//!
//! - Writes are atomic on POSIX + Windows (write to `.tmp` then
//!   rename). A crashed writer never leaves a torn file.
//! - The root directory is created lazily on first write.
//! - The schema is forward-compatible: extra unknown fields are
//!   ignored on read so future phases can grow it without breaking
//!   older `aura` binaries.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use aura_core::AgentId;
use aura_core_modes::{AgentMode, KernelMode, SpawnMode};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Stable on-disk representation of an orphan record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanRecord {
    /// Stable child agent id.
    pub agent_id: AgentId,
    /// Parent → root lineage at orphan-handoff time. Stored as
    /// strings to keep the JSON shape stable across `AgentId`
    /// representation tweaks.
    pub parent_lineage: Vec<AgentId>,
    /// Child's resolved [`AgentMode`].
    pub mode: AgentMode,
    /// Child's resolved [`KernelMode`].
    pub kernel_mode: KernelMode,
    /// Spawn mode at orphan-handoff time.
    pub spawn_mode: SpawnMode,
    /// UTC wall-clock instant the orphan was registered.
    pub spawned_at: DateTime<Utc>,
    /// Bundled subagent kind id, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Resolved model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Originating user id forwarded for audit attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub originating_user_id: Option<String>,
}

/// Errors surfaced by the orphan store.
#[derive(Debug, Error)]
pub enum OrphanStoreError {
    /// I/O failure during read / write / remove. Wrap the underlying
    /// error with the path for ergonomics.
    #[error("orphan store I/O at {path}: {source}")]
    Io {
        /// Path the I/O operation was attempting to touch.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// Serde failed parsing an on-disk record.
    #[error("orphan store: failed to parse {path}: {source}")]
    Parse {
        /// Path of the malformed file.
        path: PathBuf,
        /// Underlying serde error.
        #[source]
        source: serde_json::Error,
    },
}

/// On-disk persistence root for orphan records.
#[derive(Debug, Clone)]
pub struct OrphanStore {
    root: PathBuf,
}

impl OrphanStore {
    /// Construct an [`OrphanStore`] rooted at the given path.
    /// The directory is created lazily on first write.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Resolve the default `~/.aura/state/orphans/` path.
    ///
    /// # Errors
    ///
    /// Returns [`OrphanStoreError::Io`] if the platform home
    /// directory cannot be resolved.
    pub fn default_root() -> Result<PathBuf, OrphanStoreError> {
        let home = dirs::home_dir().ok_or_else(|| OrphanStoreError::Io {
            path: PathBuf::from("~"),
            source: io::Error::new(io::ErrorKind::NotFound, "home directory not found"),
        })?;
        Ok(home.join(".aura").join("state").join("orphans"))
    }

    /// Read-only accessor for the configured root path.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// File path for a given agent's orphan record.
    #[must_use]
    pub fn path_for(&self, agent_id: AgentId) -> PathBuf {
        self.root.join(format!("{}.json", agent_id.to_hex()))
    }

    /// Atomically write an orphan record. Creates the root directory
    /// if needed; writes to a `.tmp` neighbour then renames.
    ///
    /// # Errors
    ///
    /// Returns [`OrphanStoreError::Io`] for filesystem failures.
    pub fn write(&self, record: &OrphanRecord) -> Result<(), OrphanStoreError> {
        fs::create_dir_all(&self.root).map_err(|e| OrphanStoreError::Io {
            path: self.root.clone(),
            source: e,
        })?;
        let target = self.path_for(record.agent_id);
        let tmp = target.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| OrphanStoreError::Parse {
            path: target.clone(),
            source: e,
        })?;
        fs::write(&tmp, &bytes).map_err(|e| OrphanStoreError::Io {
            path: tmp.clone(),
            source: e,
        })?;
        fs::rename(&tmp, &target).map_err(|e| OrphanStoreError::Io {
            path: target.clone(),
            source: e,
        })?;
        info!(
            agent_id = %record.agent_id,
            path = %target.display(),
            "orphan store: wrote orphan record"
        );
        Ok(())
    }

    /// Remove an orphan record. Idempotent: a missing file returns
    /// `Ok(())`.
    ///
    /// # Errors
    ///
    /// Returns [`OrphanStoreError::Io`] for filesystem failures other
    /// than `NotFound`.
    pub fn remove(&self, agent_id: AgentId) -> Result<(), OrphanStoreError> {
        let target = self.path_for(agent_id);
        match fs::remove_file(&target) {
            Ok(()) => {
                debug!(
                    agent_id = %agent_id,
                    path = %target.display(),
                    "orphan store: removed orphan record"
                );
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(OrphanStoreError::Io {
                path: target,
                source: e,
            }),
        }
    }

    /// Read every orphan record under the root. Files that fail to
    /// parse are logged at `warn` level and skipped so a single bad
    /// file does not poison the listing.
    ///
    /// # Errors
    ///
    /// Returns [`OrphanStoreError::Io`] if the directory cannot be
    /// listed; never returns an error for individual file parse
    /// failures.
    pub fn list(&self) -> Result<Vec<OrphanRecord>, OrphanStoreError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let entries = fs::read_dir(&self.root).map_err(|e| OrphanStoreError::Io {
            path: self.root.clone(),
            source: e,
        })?;
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<OrphanRecord>(&bytes) {
                    Ok(record) => out.push(record),
                    Err(err) => warn!(
                        path = %path.display(),
                        error = %err,
                        "orphan store: skipping unparseable record"
                    ),
                },
                Err(err) => warn!(
                    path = %path.display(),
                    error = %err,
                    "orphan store: skipping unreadable record"
                ),
            }
        }
        Ok(out)
    }

    /// Load a single orphan record by agent id. Returns `Ok(None)`
    /// when no file exists; surfaces parse / I/O errors otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`OrphanStoreError::Io`] for filesystem failures other
    /// than `NotFound`, and [`OrphanStoreError::Parse`] for malformed
    /// records.
    pub fn load(&self, agent_id: AgentId) -> Result<Option<OrphanRecord>, OrphanStoreError> {
        let path = self.path_for(agent_id);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(OrphanStoreError::Io { path, source: e });
            }
        };
        let record = serde_json::from_slice::<OrphanRecord>(&bytes).map_err(|e| {
            OrphanStoreError::Parse {
                path: path.clone(),
                source: e,
            }
        })?;
        Ok(Some(record))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(agent_id: AgentId) -> OrphanRecord {
        OrphanRecord {
            agent_id,
            parent_lineage: vec![AgentId::generate()],
            mode: AgentMode::Agent,
            kernel_mode: KernelMode::AuditedLite,
            spawn_mode: SpawnMode::Detached,
            spawned_at: Utc::now(),
            kind: Some("explore".to_string()),
            model_id: Some("claude-opus-4-7".to_string()),
            originating_user_id: Some("user-root".to_string()),
        }
    }

    #[test]
    fn write_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrphanStore::new(dir.path().to_path_buf());
        let id = AgentId::generate();
        let record = make_record(id);
        store.write(&record).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], record);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrphanStore::new(dir.path().to_path_buf());
        assert!(store.load(AgentId::generate()).unwrap().is_none());
    }

    #[test]
    fn remove_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = OrphanStore::new(dir.path().to_path_buf());
        let id = AgentId::generate();
        store.remove(id).unwrap();
        store.write(&make_record(id)).unwrap();
        store.remove(id).unwrap();
        assert!(store.load(id).unwrap().is_none());
    }
}
