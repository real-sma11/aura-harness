//! Encrypt-in-place migration mechanics (Swarm TEE upgrade R2).
//!
//! [`seal_db_copy`] copies an existing **plaintext** agent-state database
//! into a fresh destination database, sealing every content-bearing value
//! with the given [`SealCipher`] on the way. Keys and pure-counter
//! metadata stay plaintext, exactly matching what a store opened with
//! `RocksStore::open_sealed(.., Some(cipher))` would have written — so the
//! copy is indistinguishable from a database that was sealed from birth.
//!
//! The atomic/resumable orchestration (temp dir, swap, `.aura-sealed`
//! marker, plaintext-backup cleanup) lives in `aura-runtime`'s
//! `state_migration` module; this module only knows how to produce a
//! sealed copy.

use std::path::Path;

use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options, WriteBatch};

use crate::cf;
use crate::error::StoreError;
use crate::seal::SealCipher;

type Db = DBWithThreadMode<MultiThreaded>;

/// Every column family the store knows about. Mirrors the descriptor
/// list in `RocksStore::open_sealed`, so a database produced by
/// [`seal_db_copy`] always carries the full current CF set even when the
/// source predates some of them.
pub const ALL_CFS: &[&str] = &[
    cf::RECORD,
    cf::AGENT_META,
    cf::INBOX,
    cf::MEMORY_FACTS,
    cf::MEMORY_EVENTS,
    cf::MEMORY_PROCEDURES,
    cf::MEMORY_EVENT_INDEX,
    cf::MEMORY_CONFIG,
    cf::AGENT_SKILLS,
    cf::RUNTIME_CAPABILITIES,
    cf::USER_TOOL_DEFAULTS,
    cf::SECRETS,
    cf::PROCESSES,
    cf::PROCESS_RUNS,
];

/// Column families whose **values** are sealed in sealed mode — the
/// content-bearing set covered by per-value sealing across the store
/// implementations (`RocksStore`, memory store, skill install store,
/// secrets vault, process store).
///
/// Deliberately excluded (plaintext in both modes, see `crate::seal`
/// module docs): `agent_meta` (pure counters/status bytes that must be
/// readable before any value is decrypted) and `memory_event_index`
/// (event-id → timestamp index with no content).
pub const SEALED_VALUE_CFS: &[&str] = &[
    cf::RECORD,
    cf::INBOX,
    cf::MEMORY_FACTS,
    cf::MEMORY_EVENTS,
    cf::MEMORY_PROCEDURES,
    cf::MEMORY_CONFIG,
    cf::AGENT_SKILLS,
    cf::RUNTIME_CAPABILITIES,
    cf::USER_TOOL_DEFAULTS,
    cf::SECRETS,
    cf::PROCESSES,
    cf::PROCESS_RUNS,
];

/// Maximum entries staged per `WriteBatch` while copying.
const COPY_BATCH_SIZE: usize = 512;

/// Outcome counters for a [`seal_db_copy`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SealCopyStats {
    /// Values encrypted on the way into the destination.
    pub values_sealed: u64,
    /// Values copied verbatim (plaintext-by-design CFs, or values that
    /// already carried the sealed envelope).
    pub values_copied: u64,
}

/// Copy the database at `src` into a fresh database at `dst`, sealing
/// every value in a [`SEALED_VALUE_CFS`] column family with `cipher`.
///
/// * `src` is opened **read-only** and never modified.
/// * `dst` must not contain an existing database (the caller deletes any
///   stale temp dir first); it is created here and flushed to disk
///   before returning, so a clean return means the sealed copy is
///   durable.
/// * Values that already carry the sealed envelope magic are copied
///   verbatim — defense in depth against double-sealing if a partially
///   sealed database is ever migrated again.
///
/// # Errors
///
/// Returns an error if either database cannot be opened or a read/write
/// fails. On error the caller discards `dst` and retries from scratch.
pub fn seal_db_copy(
    src: &Path,
    dst: &Path,
    cipher: &SealCipher,
) -> Result<SealCopyStats, StoreError> {
    // Enumerate the CFs the source actually has (an older database may
    // predate newer CFs) and open it read-only.
    let src_cf_names = Db::list_cf(&Options::default(), src)?;
    let src_db = Db::open_cf_for_read_only(&Options::default(), src, &src_cf_names, false)?;

    // The destination gets the union of the canonical CF set and the
    // source's CFs, so nothing is dropped and the sealed store finds
    // every CF it expects on the next open.
    let mut dst_cf_names: Vec<String> = ALL_CFS.iter().map(ToString::to_string).collect();
    for name in &src_cf_names {
        if name != "default" && !dst_cf_names.iter().any(|n| n == name) {
            dst_cf_names.push(name.clone());
        }
    }

    let mut dst_opts = Options::default();
    dst_opts.create_if_missing(true);
    dst_opts.create_missing_column_families(true);
    let dst_db = Db::open_cf_descriptors(
        &dst_opts,
        dst,
        dst_cf_names
            .iter()
            .map(|name| ColumnFamilyDescriptor::new(name, Options::default())),
    )?;

    let mut stats = SealCopyStats::default();

    for cf_name in &src_cf_names {
        let src_cf = src_db
            .cf_handle(cf_name)
            .ok_or_else(|| StoreError::ColumnFamilyNotFound(cf_name.clone()))?;
        let dst_cf = dst_db
            .cf_handle(cf_name)
            .ok_or_else(|| StoreError::ColumnFamilyNotFound(cf_name.clone()))?;

        let seal_values = SEALED_VALUE_CFS.contains(&cf_name.as_str());

        let mut batch = WriteBatch::default();
        let mut batched = 0usize;
        for item in src_db.iterator_cf(&src_cf, rocksdb::IteratorMode::Start) {
            let (key, value) = item?;

            if seal_values && !SealCipher::is_sealed(&value) {
                let sealed = cipher.seal(&value).map_err(|e| {
                    StoreError::Serialization(format!("sealing value during migration: {e}"))
                })?;
                batch.put_cf(&dst_cf, key, sealed);
                stats.values_sealed += 1;
            } else {
                batch.put_cf(&dst_cf, key, value);
                stats.values_copied += 1;
            }

            batched += 1;
            if batched >= COPY_BATCH_SIZE {
                dst_db.write(std::mem::take(&mut batch))?;
                batched = 0;
            }
        }
        if batched > 0 {
            dst_db.write(batch)?;
        }
    }

    // Persist everything to SSTs before returning: a clean return is the
    // caller's signal that the copy is durable and safe to swap in.
    for cf_name in &dst_cf_names {
        if let Some(handle) = dst_db.cf_handle(cf_name) {
            dst_db.flush_cf(&handle)?;
        }
    }
    dst_db.flush()?;

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RocksStore;

    fn cipher() -> SealCipher {
        SealCipher::new(&[42u8; 32])
    }

    /// Build a plaintext store with raw values in a sealed CF (record)
    /// and a plaintext-by-design CF (agent_meta), then drop it.
    fn build_plaintext_db(path: &Path) {
        let store = RocksStore::open(path, false).unwrap();
        let db = store.db_handle();
        let record_cf = db.cf_handle(cf::RECORD).unwrap();
        db.put_cf(
            &record_cf,
            b"agent/0001",
            br#"{"kind":"record","data":"secret payload"}"#,
        )
        .unwrap();
        let secrets_cf = db.cf_handle(cf::SECRETS).unwrap();
        db.put_cf(&secrets_cf, b"api-key", br#"{"value":"hunter2"}"#)
            .unwrap();
        let meta_cf = db.cf_handle(cf::AGENT_META).unwrap();
        db.put_cf(&meta_cf, b"head_seq", 7u64.to_be_bytes())
            .unwrap();
    }

    #[test]
    fn copy_seals_content_and_passes_counters_through() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = dst_dir.path().join("sealed-copy");
        build_plaintext_db(src_dir.path());

        let cipher = cipher();
        let stats = seal_db_copy(src_dir.path(), &dst, &cipher).unwrap();
        assert_eq!(stats.values_sealed, 2, "record + secret must be sealed");
        assert_eq!(stats.values_copied, 1, "agent_meta counter passes through");

        // Inspect the raw destination bytes.
        let cf_names = Db::list_cf(&Options::default(), &dst).unwrap();
        let db = Db::open_cf_for_read_only(&Options::default(), &dst, &cf_names, false).unwrap();

        let record_cf = db.cf_handle(cf::RECORD).unwrap();
        let raw = db.get_cf(&record_cf, b"agent/0001").unwrap().unwrap();
        assert!(
            SealCipher::is_sealed(&raw),
            "record value must be ciphertext"
        );
        assert_eq!(
            cipher.open(&raw).unwrap(),
            br#"{"kind":"record","data":"secret payload"}"#
        );

        let secrets_cf = db.cf_handle(cf::SECRETS).unwrap();
        let raw = db.get_cf(&secrets_cf, b"api-key").unwrap().unwrap();
        assert!(SealCipher::is_sealed(&raw));

        let meta_cf = db.cf_handle(cf::AGENT_META).unwrap();
        let raw = db.get_cf(&meta_cf, b"head_seq").unwrap().unwrap();
        assert_eq!(raw.as_ref(), 7u64.to_be_bytes(), "counters stay plaintext");
    }

    #[test]
    fn copy_is_readable_through_a_sealed_store_open() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = dst_dir.path().join("sealed-copy");

        // Write through the typed store API in plaintext mode...
        {
            let store = RocksStore::open(src_dir.path(), false).unwrap();
            use crate::store::ReadStore;
            store
                .put_user_tool_defaults("user-1", &aura_core_types::UserToolDefaults::default())
                .unwrap();
        }

        let cipher = std::sync::Arc::new(cipher());
        seal_db_copy(src_dir.path(), &dst, &cipher).unwrap();

        // ...and read it back through a sealed store over the copy.
        let sealed = RocksStore::open_sealed(&dst, false, Some(cipher)).unwrap();
        use crate::store::ReadStore;
        let defaults = sealed.get_user_tool_defaults("user-1").unwrap();
        assert!(defaults.is_some(), "sealed copy must roundtrip typed reads");
    }

    #[test]
    fn already_sealed_values_are_not_double_sealed() {
        let src_dir = tempfile::tempdir().unwrap();
        let dst_dir = tempfile::tempdir().unwrap();
        let dst = dst_dir.path().join("sealed-copy");

        let cipher = cipher();
        {
            let store = RocksStore::open(src_dir.path(), false).unwrap();
            let db = store.db_handle();
            let record_cf = db.cf_handle(cf::RECORD).unwrap();
            let pre_sealed = cipher.seal(b"already encrypted").unwrap();
            db.put_cf(&record_cf, b"k", &pre_sealed).unwrap();
        }

        let stats = seal_db_copy(src_dir.path(), &dst, &cipher).unwrap();
        assert_eq!(stats.values_sealed, 0);
        assert_eq!(stats.values_copied, 1);

        let cf_names = Db::list_cf(&Options::default(), &dst).unwrap();
        let db = Db::open_cf_for_read_only(&Options::default(), &dst, &cf_names, false).unwrap();
        let record_cf = db.cf_handle(cf::RECORD).unwrap();
        let raw = db.get_cf(&record_cf, b"k").unwrap().unwrap();
        assert_eq!(cipher.open(&raw).unwrap(), b"already encrypted");
    }
}
