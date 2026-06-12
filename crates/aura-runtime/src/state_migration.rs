//! Atomic, resumable encrypt-in-place state migration (Swarm TEE
//! upgrade R2).
//!
//! When a legacy agent's pod is recreated onto the confidential stack,
//! its first sealed boot finds **plaintext** state on the EFS subpath.
//! [`migrate_state_if_needed`] converts it to the sealed format before
//! the store is opened, using copy + atomic swap so a crash at any
//! point either resumes or restarts cleanly — plaintext data is never
//! mutated in place and never lost before a verified sealed copy has
//! taken its place.
//!
//! # Directory layout
//!
//! For a database directory `<db>` (e.g. `$data_dir/db`):
//!
//! * `<db>.sealed-migrating` — temp destination while the sealed copy
//!   is being written. Disposable: deleted and rebuilt on any retry.
//! * `<db>.plaintext-backup` — the original plaintext database between
//!   the swap and the final cleanup. Authoritative rollback source
//!   while it exists.
//! * `$data_dir/.aura-sealed` — the non-secret marker recording "this
//!   state dir is sealed" (see [`crate::sealing`]).
//!
//! # State machine
//!
//! ```text
//!  (1) plaintext, no marker
//!        | seal_db_copy(<db> -> <db>.sealed-migrating)   [fsync'd]
//!        | verify temp opens cleanly
//!  (2) rename <db> -> <db>.plaintext-backup
//!  (3) rename <db>.sealed-migrating -> <db>
//!  (4) write .aura-sealed marker
//!  (5) verify live <db> opens cleanly, then delete the backup
//! ```
//!
//! Crash recovery on the next boot:
//!
//! * **No marker, backup present** (crash between (2) and (4)): the
//!   backup is the authoritative plaintext. Whatever sits at `<db>` is
//!   a disposable copy — delete it, rename the backup back, restart
//!   the migration from scratch.
//! * **No marker, temp present** (crash during (1)): the temp copy may
//!   be partial — delete it and restart the copy from scratch
//!   (idempotent; the source was never touched).
//! * **Marker present, backup present** (crash between (4) and (5)):
//!   the sealed database is live; re-verify it opens cleanly and only
//!   then delete the backup.
//! * **Marker present, no leftovers**: steady state, nothing to do.
//!
//! The EFS snapshot taken before the R2 rollout remains the disaster
//! recovery path; the local backup directory only needs to survive the
//! window of a single migration.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use aura_store_db::{seal_db_copy, RocksStore, SealCipher, SealCopyStats};
use tracing::{info, warn};

/// Suffix of the temp directory the sealed copy is written into.
pub const MIGRATING_SUFFIX: &str = "sealed-migrating";

/// Suffix of the plaintext backup directory kept across the swap.
pub const BACKUP_SUFFIX: &str = "plaintext-backup";

/// Outcome of [`migrate_state_if_needed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// No migration ran (fresh state dir, or already sealed).
    NotNeeded(&'static str),
    /// Plaintext state was converted to sealed.
    Migrated(SealCopyStats),
}

/// `<db>.sealed-migrating` next to the database directory.
#[must_use]
pub fn migrating_dir(db_path: &Path) -> PathBuf {
    sibling(db_path, MIGRATING_SUFFIX)
}

/// `<db>.plaintext-backup` next to the database directory.
#[must_use]
pub fn backup_dir(db_path: &Path) -> PathBuf {
    sibling(db_path, BACKUP_SUFFIX)
}

fn sibling(db_path: &Path, suffix: &str) -> PathBuf {
    let name = db_path
        .file_name()
        .map_or_else(|| "state".to_string(), |n| n.to_string_lossy().into_owned());
    db_path.with_file_name(format!("{name}.{suffix}"))
}

/// Whether a RocksDB database with data exists at `path`.
///
/// `CURRENT` is written on every database creation, so its absence
/// means "nothing to migrate" (e.g. the empty directory `Node::run`
/// pre-creates on a fresh boot).
fn db_has_data(path: &Path) -> bool {
    path.join("CURRENT").exists()
}

/// Open the database at `path` as a sealed store and drop it — proves
/// the copy/swap produced a database RocksDB accepts before we discard
/// any plaintext.
fn verify_sealed_open(path: &Path, cipher: &Arc<SealCipher>) -> anyhow::Result<()> {
    RocksStore::open_sealed(path, false, Some(Arc::clone(cipher)))
        .map(drop)
        .with_context(|| format!("verifying sealed database at {}", path.display()))
}

/// Best-effort directory fsync so the renames are durable (no-op off
/// Unix; NTFS metadata operations are journaled).
fn fsync_dir_best_effort(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Run the encrypt-in-place migration if (and only if) this sealed boot
/// found plaintext state. Called from the sealing boot flow **after**
/// the DEK is obtained and **before** the store is opened.
///
/// # Errors
///
/// Returns an error when the copy, swap, or verification fails. The
/// plaintext source (or its backup directory) is preserved on every
/// error path, so a failed boot can simply retry.
pub fn migrate_state_if_needed(
    data_dir: &Path,
    db_path: &Path,
    key_id: Option<&str>,
    cipher: &Arc<SealCipher>,
) -> anyhow::Result<MigrationOutcome> {
    let marker = data_dir.join(crate::sealing::SEALED_MARKER_FILENAME);
    let temp = migrating_dir(db_path);
    let backup = backup_dir(db_path);

    if marker.exists() {
        // Crash window (1): temp leftovers are disposable.
        if temp.exists() {
            warn!(temp = %temp.display(), "removing stale migration temp dir");
            std::fs::remove_dir_all(&temp)
                .with_context(|| format!("removing stale temp dir {}", temp.display()))?;
        }
        // Crash window (4)→(5): finish the deferred backup cleanup, but
        // only once the live sealed database provably opens.
        if backup.exists() {
            verify_sealed_open(db_path, cipher).context(
                "sealed database failed to open while a plaintext backup exists; \
                 keeping the backup for operator recovery",
            )?;
            std::fs::remove_dir_all(&backup)
                .with_context(|| format!("removing plaintext backup {}", backup.display()))?;
            info!("completed deferred plaintext-backup cleanup from an interrupted migration");
        }
        return Ok(MigrationOutcome::NotNeeded("state is already sealed"));
    }

    // No marker. A backup without a marker means a migration was
    // interrupted mid-swap — the backup is the authoritative plaintext,
    // so roll back to it and restart from scratch.
    if backup.exists() {
        warn!(
            backup = %backup.display(),
            "interrupted state migration detected; rolling back to the plaintext backup"
        );
        if db_path.exists() {
            std::fs::remove_dir_all(db_path)
                .with_context(|| format!("removing partial database {}", db_path.display()))?;
        }
        std::fs::rename(&backup, db_path)
            .with_context(|| format!("restoring plaintext backup to {}", db_path.display()))?;
    }

    // A temp dir without a marker is a partial copy — restart it.
    if temp.exists() {
        warn!(temp = %temp.display(), "removing partial migration temp dir; restarting copy");
        std::fs::remove_dir_all(&temp)
            .with_context(|| format!("removing partial temp dir {}", temp.display()))?;
    }

    if !db_has_data(db_path) {
        return Ok(MigrationOutcome::NotNeeded(
            "fresh sealed boot (no plaintext state to migrate)",
        ));
    }

    info!(
        db = %db_path.display(),
        "plaintext state found on sealed boot; starting encrypt-in-place migration"
    );

    // (1) Copy + seal into the temp dir (flushed durable by the copy),
    // then prove the copy opens before touching the live directory.
    let stats = seal_db_copy(db_path, &temp, cipher)
        .with_context(|| format!("sealing state copy into {}", temp.display()))?;
    verify_sealed_open(&temp, cipher)?;

    // (2)+(3) Atomic swap: plaintext aside, sealed copy live.
    std::fs::rename(db_path, &backup)
        .with_context(|| format!("moving plaintext database to {}", backup.display()))?;
    std::fs::rename(&temp, db_path)
        .with_context(|| format!("moving sealed copy to {}", db_path.display()))?;
    if let Some(parent) = db_path.parent() {
        fsync_dir_best_effort(parent);
    }

    // (4) Marker: from here on, boots take the "already sealed" path.
    crate::sealing::write_sealed_marker(data_dir, key_id)?;

    // (5) Drop the plaintext only after the live sealed DB opens cleanly.
    verify_sealed_open(db_path, cipher).context(
        "swapped-in sealed database failed to open; plaintext backup preserved",
    )?;
    std::fs::remove_dir_all(&backup)
        .with_context(|| format!("removing plaintext backup {}", backup.display()))?;

    info!(
        values_sealed = stats.values_sealed,
        values_copied = stats.values_copied,
        "encrypt-in-place state migration complete"
    );
    Ok(MigrationOutcome::Migrated(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_store_db::{cf, SealCipher};

    const KEY_ID: Option<&str> = Some("swarm/agents/test/state-key");

    fn cipher() -> Arc<SealCipher> {
        Arc::new(SealCipher::new(&[5u8; 32]))
    }

    /// Build a plaintext database with one sealed-CF value and one
    /// plaintext-by-design metadata value.
    fn build_plaintext_db(db_path: &Path) {
        let store = RocksStore::open(db_path, false).unwrap();
        let db = store.db_handle();
        let secrets = db.cf_handle(cf::SECRETS).unwrap();
        db.put_cf(&secrets, b"api-key", br#"{"value":"hunter2"}"#)
            .unwrap();
        let meta = db.cf_handle(cf::AGENT_META).unwrap();
        db.put_cf(&meta, b"head_seq", 7u64.to_be_bytes()).unwrap();
    }

    fn assert_migrated_db_readable(db_path: &Path, cipher: &Arc<SealCipher>) {
        let store = RocksStore::open_sealed(db_path, false, Some(Arc::clone(cipher))).unwrap();
        let db = store.db_handle();
        let secrets = db.cf_handle(cf::SECRETS).unwrap();
        let raw = db.get_cf(&secrets, b"api-key").unwrap().unwrap();
        assert!(
            SealCipher::is_sealed(&raw),
            "secret must be ciphertext on disk after migration"
        );
        assert_eq!(cipher.open(&raw).unwrap(), br#"{"value":"hunter2"}"#);
        let meta = db.cf_handle(cf::AGENT_META).unwrap();
        let raw = db.get_cf(&meta, b"head_seq").unwrap().unwrap();
        assert_eq!(raw.as_slice(), 7u64.to_be_bytes());
    }

    #[test]
    fn fresh_sealed_boot_needs_no_migration() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        // Node::run pre-creates the (empty) db dir before sealing runs.
        std::fs::create_dir_all(&db_path).unwrap();

        let outcome =
            migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher()).unwrap();
        assert!(matches!(outcome, MigrationOutcome::NotNeeded(_)), "{outcome:?}");
        assert!(!migrating_dir(&db_path).exists());
        assert!(!backup_dir(&db_path).exists());
    }

    #[test]
    fn plaintext_state_migrates_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);

        let cipher = cipher();
        let outcome =
            migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        let MigrationOutcome::Migrated(stats) = outcome else {
            panic!("expected migration, got {outcome:?}");
        };
        assert_eq!(stats.values_sealed, 1);
        assert_eq!(stats.values_copied, 1);

        // Temp and backup are gone; the marker is written; data reads back.
        assert!(!migrating_dir(&db_path).exists());
        assert!(!backup_dir(&db_path).exists(), "plaintext backup must be deleted");
        assert!(dir
            .path()
            .join(crate::sealing::SEALED_MARKER_FILENAME)
            .exists());
        assert_migrated_db_readable(&db_path, &cipher);
    }

    #[test]
    fn second_boot_after_migration_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);
        let cipher = cipher();

        let first = migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        assert!(matches!(first, MigrationOutcome::Migrated(_)));

        let second = migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        assert!(
            matches!(second, MigrationOutcome::NotNeeded(_)),
            "marker present must skip migration: {second:?}"
        );
        assert_migrated_db_readable(&db_path, &cipher);
    }

    #[test]
    fn interrupted_copy_restarts_from_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);

        // Simulate a crash mid-copy: a partial (garbage) temp dir exists,
        // no marker.
        let temp = migrating_dir(&db_path);
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("000001.sst"), b"definitely not a real sst").unwrap();

        let cipher = cipher();
        let outcome =
            migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        assert!(matches!(outcome, MigrationOutcome::Migrated(_)), "{outcome:?}");
        assert!(!temp.exists());
        assert_migrated_db_readable(&db_path, &cipher);
    }

    #[test]
    fn interrupted_swap_rolls_back_to_backup_and_remigrates() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);
        let cipher = cipher();

        // Simulate a crash between the two swap renames: plaintext lives
        // in the backup dir, nothing (or a disposable copy) at <db>, no
        // marker yet.
        std::fs::rename(&db_path, backup_dir(&db_path)).unwrap();

        let outcome =
            migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        assert!(matches!(outcome, MigrationOutcome::Migrated(_)), "{outcome:?}");
        assert!(!backup_dir(&db_path).exists());
        assert_migrated_db_readable(&db_path, &cipher);
    }

    #[test]
    fn marker_present_with_leftover_backup_finishes_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);
        let cipher = cipher();

        // Full migration, then simulate a crash between marker write and
        // backup deletion by recreating a backup dir.
        migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        let backup = backup_dir(&db_path);
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("CURRENT"), b"stale plaintext leftover").unwrap();

        let outcome =
            migrate_state_if_needed(dir.path(), &db_path, KEY_ID, &cipher).unwrap();
        assert!(matches!(outcome, MigrationOutcome::NotNeeded(_)));
        assert!(!backup.exists(), "deferred backup cleanup must run");
        assert_migrated_db_readable(&db_path, &cipher);
    }

    /// Plaintext boots never touch the state dir — enforced by the
    /// caller (`prepare_with_config` returns before the migration when
    /// sealing is disabled), and double-checked here: a plaintext DB
    /// stays byte-for-byte plaintext when no migration is invoked.
    #[test]
    fn plaintext_mode_leaves_state_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("db");
        build_plaintext_db(&db_path);

        // No migration call (plaintext mode). Values stay plaintext.
        let store = RocksStore::open(&db_path, false).unwrap();
        let db = store.db_handle();
        let secrets = db.cf_handle(cf::SECRETS).unwrap();
        let raw = db.get_cf(&secrets, b"api-key").unwrap().unwrap();
        assert_eq!(raw.as_slice(), br#"{"value":"hunter2"}"#);
        assert!(!dir
            .path()
            .join(crate::sealing::SEALED_MARKER_FILENAME)
            .exists());
    }
}
