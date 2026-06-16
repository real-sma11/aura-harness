//! In-TEE secrets vault (Swarm TEE upgrade phase 6).
//!
//! Named secrets (`name → value + metadata`) persisted in the `secrets`
//! column family of the shared agent-state RocksDB. Storage goes through
//! the same per-value sealing envelope as every other content-bearing
//! store (see [`crate::seal`]):
//!
//! * **Sealed mode** (`AURA_STATE_ENCRYPTION=sealed`): secret records are
//!   AES-256-GCM encrypted under the per-agent DEK released by the KBS
//!   after attestation — values are never written to disk in plaintext.
//! * **Plaintext/dev mode**: records are stored as plain JSON, exactly
//!   like the rest of the state (documented v1 tradeoff; local dev runs
//!   without an attestation stack).
//!
//! # Redaction contract
//!
//! [`SecretRecord`] carries the secret value and therefore has a manual
//! `Debug` impl that redacts it — the value can never leak through
//! `{:?}` formatting in logs or traces. Every read API that is not an
//! explicit reveal returns [`SecretMetadata`], which does not contain
//! the value at all.
//!
//! # Run injection (v1 scope)
//!
//! Secrets are *not* automatically injected into agent run environments
//! in v1 — there is no central env/config injection point for runs in
//! this codebase today. In-VM tooling reads secrets through the harness
//! HTTP API (`GET /secrets/:name?reveal=true`).

use crate::seal::SealCipher;
use chrono::{DateTime, Utc};
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Maximum length of a secret name.
pub const MAX_SECRET_NAME_LEN: usize = 128;

/// Maximum size of a secret value in bytes.
pub const MAX_SECRET_VALUE_BYTES: usize = 8 * 1024;

/// Errors produced by [`SecretsVault`].
#[derive(Debug, thiserror::Error)]
pub enum VaultError {
    /// The secret name is empty, too long, or contains invalid characters.
    #[error("invalid secret name: {0}")]
    InvalidName(String),
    /// The secret value exceeds [`MAX_SECRET_VALUE_BYTES`].
    #[error("secret value too large ({len} bytes; max {max})")]
    ValueTooLarge {
        /// Size of the rejected value.
        len: usize,
        /// The enforced ceiling.
        max: usize,
    },
    /// Underlying RocksDB / sealing failure.
    #[error("vault store error: {0}")]
    Store(String),
    /// JSON (de)serialization failure.
    #[error("vault serialization error: {0}")]
    Serde(String),
}

/// A stored secret: value plus metadata.
///
/// Deliberately **no derived `Debug`** — the manual impl below redacts
/// the value so the secret cannot leak through log/tracing formatting.
#[derive(Clone, Serialize, Deserialize)]
pub struct SecretRecord {
    /// Unique secret name (the store key).
    pub name: String,
    /// The secret value. Only exposed through explicit reveal reads.
    pub value: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Creation timestamp (preserved across updates).
    pub created_at: DateTime<Utc>,
    /// Last-update timestamp.
    pub updated_at: DateTime<Utc>,
}

impl std::fmt::Debug for SecretRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRecord")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("description", &self.description)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .finish()
    }
}

impl SecretRecord {
    /// The value-free view of this record (safe to list/log/serialize).
    #[must_use]
    pub fn metadata(&self) -> SecretMetadata {
        SecretMetadata {
            name: self.name.clone(),
            description: self.description.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Value-free secret metadata, safe for listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretMetadata {
    /// Secret name.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last-update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Validate a secret name: non-empty, bounded, `[A-Za-z0-9._-]` only.
///
/// # Errors
/// Returns [`VaultError::InvalidName`] when the name violates the rules.
pub fn validate_secret_name(name: &str) -> Result<(), VaultError> {
    if name.is_empty() {
        return Err(VaultError::InvalidName("name must not be empty".into()));
    }
    if name.len() > MAX_SECRET_NAME_LEN {
        return Err(VaultError::InvalidName(format!(
            "name exceeds {MAX_SECRET_NAME_LEN} characters"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(VaultError::InvalidName(
            "name may only contain ASCII letters, digits, '-', '_', '.'".into(),
        ));
    }
    Ok(())
}

/// Named-secrets vault backed by the shared agent-state RocksDB.
///
/// Construction mirrors [`SkillInstallStore`-style sealed stores]: a
/// shared DB handle plus the optional value cipher decided at boot by
/// `prepare_state_sealing`.
///
/// [`SkillInstallStore`-style sealed stores]: crate::seal
pub struct SecretsVault {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
    /// Optional value-sealing cipher. `Some` in sealed mode (values are
    /// AES-256-GCM ciphertext at rest); `None` is plaintext/dev mode.
    cipher: Option<Arc<SealCipher>>,
}

impl SecretsVault {
    /// Create a plaintext-mode vault on the given shared database handle.
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db, cipher: None }
    }

    /// Create a vault with optional sealed (encrypted-at-rest) values.
    #[must_use]
    pub const fn with_cipher(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        cipher: Option<Arc<SealCipher>>,
    ) -> Self {
        Self { db, cipher }
    }

    fn seal_value(&self, plain: Vec<u8>) -> Result<Vec<u8>, VaultError> {
        match &self.cipher {
            Some(cipher) => cipher
                .seal(&plain)
                .map_err(|e| VaultError::Store(format!("sealing value: {e}"))),
            None => Ok(plain),
        }
    }

    fn open_value<'a>(&self, bytes: &'a [u8]) -> Result<std::borrow::Cow<'a, [u8]>, VaultError> {
        match &self.cipher {
            Some(cipher) => cipher
                .open(bytes)
                .map(std::borrow::Cow::Owned)
                .map_err(|e| VaultError::Store(format!("opening sealed value: {e}"))),
            None => Ok(std::borrow::Cow::Borrowed(bytes)),
        }
    }

    fn cf_handle(&self) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, VaultError> {
        self.db
            .cf_handle(crate::cf::SECRETS)
            .ok_or_else(|| VaultError::Store("secrets column family not found".into()))
    }

    /// Create or update a named secret. `created_at` is preserved on
    /// update; `updated_at` is always refreshed.
    ///
    /// # Errors
    /// Rejects invalid names and oversized values; surfaces store errors.
    pub fn put(
        &self,
        name: &str,
        value: String,
        description: Option<String>,
    ) -> Result<SecretMetadata, VaultError> {
        validate_secret_name(name)?;
        if value.len() > MAX_SECRET_VALUE_BYTES {
            return Err(VaultError::ValueTooLarge {
                len: value.len(),
                max: MAX_SECRET_VALUE_BYTES,
            });
        }

        let now = Utc::now();
        let created_at = self.get(name)?.map_or(now, |existing| existing.created_at);
        let record = SecretRecord {
            name: name.to_string(),
            value,
            description,
            created_at,
            updated_at: now,
        };

        let plain = serde_json::to_vec(&record).map_err(|e| VaultError::Serde(e.to_string()))?;
        let sealed = self.seal_value(plain)?;
        let cf = self.cf_handle()?;
        self.db
            .put_cf(&cf, name.as_bytes(), sealed)
            .map_err(|e| VaultError::Store(e.to_string()))?;
        Ok(record.metadata())
    }

    /// Fetch the full record (including the value) for `name`.
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn get(&self, name: &str) -> Result<Option<SecretRecord>, VaultError> {
        validate_secret_name(name)?;
        let cf = self.cf_handle()?;
        match self
            .db
            .get_cf(&cf, name.as_bytes())
            .map_err(|e| VaultError::Store(e.to_string()))?
        {
            Some(bytes) => {
                let bytes = self.open_value(&bytes)?;
                let record: SecretRecord =
                    serde_json::from_slice(&bytes).map_err(|e| VaultError::Serde(e.to_string()))?;
                Ok(Some(record))
            }
            None => Ok(None),
        }
    }

    /// Delete a secret. Returns `true` when a record existed.
    ///
    /// # Errors
    /// Surfaces store failures.
    pub fn delete(&self, name: &str) -> Result<bool, VaultError> {
        validate_secret_name(name)?;
        let existed = self.get(name)?.is_some();
        if existed {
            let cf = self.cf_handle()?;
            self.db
                .delete_cf(&cf, name.as_bytes())
                .map_err(|e| VaultError::Store(e.to_string()))?;
        }
        Ok(existed)
    }

    /// List all secrets as value-free metadata, sorted by name (the
    /// natural RocksDB key order).
    ///
    /// # Errors
    /// Surfaces store / unsealing / decoding failures.
    pub fn list(&self) -> Result<Vec<SecretMetadata>, VaultError> {
        let cf = self.cf_handle()?;
        let mut out = Vec::new();
        for item in self.db.iterator_cf(&cf, IteratorMode::Start) {
            let (_, v) = item.map_err(|e| VaultError::Store(e.to_string()))?;
            let bytes = self.open_value(&v)?;
            let record: SecretRecord =
                serde_json::from_slice(&bytes).map_err(|e| VaultError::Serde(e.to_string()))?;
            out.push(record.metadata());
        }
        Ok(out)
    }
}

impl std::fmt::Debug for SecretsVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose contents or key material.
        f.debug_struct("SecretsVault")
            .field("sealed", &self.cipher.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::{ColumnFamilyDescriptor, Options};

    fn test_db(dir: &std::path::Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![ColumnFamilyDescriptor::new(
            crate::cf::SECRETS,
            Options::default(),
        )];
        Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
    }

    #[test]
    fn crud_roundtrip_plaintext() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SecretsVault::new(test_db(dir.path()));

        let meta = vault
            .put("api-key", "s3cr3t-value".into(), Some("CI key".into()))
            .unwrap();
        assert_eq!(meta.name, "api-key");
        assert_eq!(meta.description.as_deref(), Some("CI key"));

        let record = vault.get("api-key").unwrap().unwrap();
        assert_eq!(record.value, "s3cr3t-value");
        assert_eq!(record.created_at, meta.created_at);

        // Update preserves created_at and bumps updated_at.
        let updated = vault.put("api-key", "rotated".into(), None).unwrap();
        assert_eq!(updated.created_at, meta.created_at);
        assert!(updated.updated_at >= meta.updated_at);
        assert_eq!(vault.get("api-key").unwrap().unwrap().value, "rotated");

        assert!(vault.delete("api-key").unwrap());
        assert!(vault.get("api-key").unwrap().is_none());
        assert!(!vault.delete("api-key").unwrap());
    }

    #[test]
    fn list_returns_metadata_only_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SecretsVault::new(test_db(dir.path()));
        vault.put("b-token", "value-b".into(), None).unwrap();
        vault
            .put("a-token", "value-a".into(), Some("first".into()))
            .unwrap();

        let listed = vault.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "a-token");
        assert_eq!(listed[1].name, "b-token");
        // SecretMetadata structurally has no value field; double-check the
        // serialized form never carries one either.
        let json = serde_json::to_string(&listed).unwrap();
        assert!(!json.contains("value-a") && !json.contains("value-b"));
    }

    /// Sealed mode: value bytes must not appear in plaintext anywhere in
    /// the raw on-disk column family (same pattern as the skill-install
    /// and record-entry sealed-at-rest tests).
    #[test]
    fn sealed_at_rest_value_not_plaintext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let cipher = Arc::new(SealCipher::new(&[9u8; 32]));
        let vault = SecretsVault::with_cipher(Arc::clone(&db), Some(cipher));

        let secret_value = b"hunter2-super-secret-token";
        vault
            .put(
                "db-password",
                String::from_utf8(secret_value.to_vec()).unwrap(),
                None,
            )
            .unwrap();

        // Roundtrip still works through the vault.
        assert_eq!(
            vault.get("db-password").unwrap().unwrap().value.as_bytes(),
            secret_value
        );

        // Raw CF bytes carry the sealed envelope, not the plaintext.
        let cf = db.cf_handle(crate::cf::SECRETS).unwrap();
        let raw = db
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert!(SealCipher::is_sealed(&raw));
        assert!(
            !raw.windows(secret_value.len())
                .any(|w| w == secret_value.as_slice()),
            "sealed bytes must not contain the plaintext value"
        );
    }

    /// Wrong DEK must fail to open, not return garbage.
    #[test]
    fn sealed_wrong_key_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db(dir.path());
        let vault =
            SecretsVault::with_cipher(Arc::clone(&db), Some(Arc::new(SealCipher::new(&[1u8; 32]))));
        vault.put("k", "v".into(), None).unwrap();

        let other = SecretsVault::with_cipher(db, Some(Arc::new(SealCipher::new(&[2u8; 32]))));
        assert!(other.get("k").is_err());
    }

    #[test]
    fn debug_redacts_value() {
        let record = SecretRecord {
            name: "token".into(),
            value: "tippy-top-secret".into(),
            description: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let rendered = format!("{record:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("tippy-top-secret"));
    }

    #[test]
    fn name_validation() {
        assert!(validate_secret_name("api.key_2-prod").is_ok());
        assert!(validate_secret_name("").is_err());
        assert!(validate_secret_name(&"a".repeat(MAX_SECRET_NAME_LEN + 1)).is_err());
        assert!(validate_secret_name("../escape").is_err());
        assert!(validate_secret_name("with space").is_err());
        assert!(validate_secret_name("emoji🐛").is_err());
    }

    #[test]
    fn oversized_value_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let vault = SecretsVault::new(test_db(dir.path()));
        let huge = "x".repeat(MAX_SECRET_VALUE_BYTES + 1);
        assert!(matches!(
            vault.put("big", huge, None),
            Err(VaultError::ValueTooLarge { .. })
        ));
    }
}
