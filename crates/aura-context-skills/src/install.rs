//! Per-agent skill installation tracking backed by `RocksDB`.
//!
//! Each agent has its own set of installed skills, stored as JSON-encoded
//! [`SkillInstallation`] values under the `agent_skills` column family.
//! Keys are `{agent_id}\0{skill_name}` so prefix iteration can list all
//! skills for a single agent.

use crate::error::SkillError;
use aura_core::AgentId;
use chrono::{DateTime, Utc};
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Record of a skill installed for a specific agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallation {
    /// The agent this skill is installed for.
    pub agent_id: AgentId,
    /// Name of the installed skill.
    pub skill_name: String,
    /// Optional URL the skill was installed from.
    pub source_url: Option<String>,
    /// Timestamp of installation.
    pub installed_at: DateTime<Utc>,
    /// Optional version string.
    pub version: Option<String>,
    /// Filesystem paths the user approved at install time.
    #[serde(default)]
    pub approved_paths: Vec<String>,
    /// Shell commands the user approved at install time.
    #[serde(default)]
    pub approved_commands: Vec<String>,
}

/// Abstraction over the skill installation store for testability.
pub trait SkillInstallStoreApi: Send + Sync {
    /// Install a skill for an agent.
    fn install(&self, installation: &SkillInstallation) -> Result<(), SkillError>;
    /// Uninstall a skill for an agent.
    fn uninstall(&self, agent_id: AgentId, skill_name: &str) -> Result<(), SkillError>;
    /// List all skills installed for an agent.
    fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<SkillInstallation>, SkillError>;
    /// Check if a skill is installed for an agent.
    fn is_installed(&self, agent_id: AgentId, skill_name: &str) -> Result<bool, SkillError>;
}

/// Tracks per-agent skill installations in `RocksDB`.
pub struct SkillInstallStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
}

impl SkillInstallStore {
    /// Create a new store backed by the given shared database handle.
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db }
    }

    fn cf_handle(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, SkillError> {
        self.db
            .cf_handle(aura_store::cf::AGENT_SKILLS)
            .ok_or_else(|| {
                SkillError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "agent_skills column family not found",
                ))
            })
    }

    fn key(agent_id: AgentId, skill_name: &str) -> Vec<u8> {
        let hex = agent_id.to_hex();
        let mut key = Vec::with_capacity(hex.len() + 1 + skill_name.len());
        key.extend_from_slice(hex.as_bytes());
        key.push(0);
        key.extend_from_slice(skill_name.as_bytes());
        key
    }

    fn agent_prefix(agent_id: AgentId) -> Vec<u8> {
        let hex = agent_id.to_hex();
        let mut prefix = Vec::with_capacity(hex.len() + 1);
        prefix.extend_from_slice(hex.as_bytes());
        prefix.push(0);
        prefix
    }
}

impl SkillInstallStoreApi for SkillInstallStore {
    fn install(&self, installation: &SkillInstallation) -> Result<(), SkillError> {
        let cf = self.cf_handle()?;
        let key = Self::key(installation.agent_id, &installation.skill_name);
        let value =
            serde_json::to_vec(installation).map_err(|e| SkillError::Parse(e.to_string()))?;
        self.db
            .put_cf(&cf, key, value)
            .map_err(|e| SkillError::Store(e.to_string()))
    }

    fn uninstall(&self, agent_id: AgentId, skill_name: &str) -> Result<(), SkillError> {
        let cf = self.cf_handle()?;
        let key = Self::key(agent_id, skill_name);
        self.db
            .delete_cf(&cf, key)
            .map_err(|e| SkillError::Store(e.to_string()))
    }

    fn list_for_agent(&self, agent_id: AgentId) -> Result<Vec<SkillInstallation>, SkillError> {
        let cf = self.cf_handle()?;
        let prefix = Self::agent_prefix(agent_id);
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        let mut installations = Vec::new();
        for item in iter {
            let (k, v) = item.map_err(|e| SkillError::Store(e.to_string()))?;
            if !k.starts_with(&prefix) {
                break;
            }
            let record: SkillInstallation =
                serde_json::from_slice(&v).map_err(|e| SkillError::Parse(e.to_string()))?;
            installations.push(record);
        }
        Ok(installations)
    }

    fn is_installed(&self, agent_id: AgentId, skill_name: &str) -> Result<bool, SkillError> {
        let cf = self.cf_handle()?;
        let key = Self::key(agent_id, skill_name);
        let exists = self
            .db
            .get_cf(&cf, key)
            .map_err(|e| SkillError::Store(e.to_string()))?;
        Ok(exists.is_some())
    }
}
