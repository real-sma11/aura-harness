//! Per-agent skill installation tracking backed by `RocksDB`.
//!
//! Each agent has its own set of installed skills, stored as JSON-encoded
//! [`SkillInstallation`] values under the `agent_skills` column family.
//! Keys are `{agent_id}\0{skill_name}` so prefix iteration can list all
//! skills for a single agent.

use crate::error::SkillError;
use aura_core_types::AgentId;
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
    /// Optional value-sealing cipher (Swarm TEE phase 5). When `Some`,
    /// installation records are AES-256-GCM sealed at rest; `None`
    /// keeps the legacy plaintext JSON format byte-for-byte.
    cipher: Option<Arc<aura_store_db::SealCipher>>,
}

impl SkillInstallStore {
    /// Create a new store backed by the given shared database handle.
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db, cipher: None }
    }

    /// Create a store with optional sealed (encrypted-at-rest) values.
    #[must_use]
    pub const fn with_cipher(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        cipher: Option<Arc<aura_store_db::SealCipher>>,
    ) -> Self {
        Self { db, cipher }
    }

    fn seal_value(&self, plain: Vec<u8>) -> Result<Vec<u8>, SkillError> {
        match &self.cipher {
            Some(cipher) => cipher
                .seal(&plain)
                .map_err(|e| SkillError::Store(format!("sealing value: {e}"))),
            None => Ok(plain),
        }
    }

    fn open_value<'a>(&self, bytes: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, SkillError> {
        match &self.cipher {
            Some(cipher) => cipher
                .open(bytes)
                .map(std::borrow::Cow::Owned)
                .map_err(|e| SkillError::Store(format!("opening sealed value: {e}"))),
            None => Ok(std::borrow::Cow::Borrowed(bytes)),
        }
    }

    fn cf_handle(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, SkillError> {
        self.db
            .cf_handle(aura_store_db::cf::AGENT_SKILLS)
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
        let value = self.seal_value(value)?;
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
            let record: SkillInstallation = serde_json::from_slice(&self.open_value(&v)?)
                .map_err(|e| SkillError::Parse(e.to_string()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use aura_store_db::SealCipher;
    use rocksdb::{ColumnFamilyDescriptor, Options};

    fn test_db(dir: &std::path::Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![ColumnFamilyDescriptor::new(
            aura_store_db::cf::AGENT_SKILLS,
            Options::default(),
        )];
        Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
    }

    fn make_installation(agent_id: AgentId) -> SkillInstallation {
        SkillInstallation {
            agent_id,
            skill_name: "deploy".to_string(),
            source_url: Some("https://example.com/deploy".to_string()),
            installed_at: Utc::now(),
            version: Some("1.0.0".to_string()),
            approved_paths: vec!["/workspace".to_string()],
            approved_commands: vec!["kubectl".to_string()],
        }
    }

    /// Sealed mode (Swarm TEE phase 5): installation records roundtrip
    /// through the cipher and the on-disk bytes are ciphertext.
    #[test]
    fn sealed_install_roundtrip_and_ciphertext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let cipher = Arc::new(SealCipher::new(&[3u8; 32]));
        let store = SkillInstallStore::with_cipher(Arc::clone(&db), Some(cipher));

        let agent = AgentId::generate();
        store.install(&make_installation(agent)).unwrap();

        assert!(store.is_installed(agent, "deploy").unwrap());
        let listed = store.list_for_agent(agent).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].skill_name, "deploy");

        let cf = db.cf_handle(aura_store_db::cf::AGENT_SKILLS).unwrap();
        let raw = db
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert!(SealCipher::is_sealed(&raw));
    }

    /// Plaintext mode stays byte-for-byte the legacy JSON format.
    #[test]
    fn plaintext_install_on_disk_format_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let store = SkillInstallStore::new(Arc::clone(&db));

        let agent = AgentId::generate();
        let installation = make_installation(agent);
        store.install(&installation).unwrap();

        let cf = db.cf_handle(aura_store_db::cf::AGENT_SKILLS).unwrap();
        let raw = db
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(raw.to_vec(), serde_json::to_vec(&installation).unwrap());
        assert!(!SealCipher::is_sealed(&raw));
    }
}
