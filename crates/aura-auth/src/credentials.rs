//! OS-native credential storage with a file-based fallback.
//!
//! Persists authentication sessions in the platform credential store —
//! DPAPI on Windows, the Keychain on macOS, and the Secret Service
//! (libsecret / gnome-keyring / KWallet) on Linux — so the JWT is
//! protected by the user's OS login rather than written to disk in
//! plaintext.
//!
//! When the platform credential service is unavailable (headless Linux
//! without a running secret-service daemon, for example), we fall back
//! to `~/.aura/credentials.json`. The write is **atomic** (tmp-file +
//! rename) and the file inherits restrictive permissions — `0600` on
//! Unix; on Windows the tmp file is opened with `share_mode(0)` so
//! concurrent opens are blocked for the duration of the write. The
//! fallback path is logged at WARN so operators can see why secrets
//! landed on disk. (Wave 5 / T5, hardened in security Phase 7.)
//!
//! ## Security notes
//!
//! - **Atomicity.** A crash mid-write cannot leave a truncated JSON
//!   that looks like a corrupted session: the final file is either the
//!   previous version or the new one. See [`atomic_write_private`].
//! - **Plaintext at rest.** The fallback file is still plaintext. Full
//!   encryption-at-rest (DPAPI / `age`) is a follow-up; this phase
//!   addresses atomicity + permissions + visibility, which are the
//!   highest-impact issues from the H4 audit finding. No `age` or
//!   `chacha20poly1305` crate exists in the workspace dep graph, so
//!   we deliberately do *not* pull one in here.
//! - **Windows ACL.** `share_mode(0)` blocks concurrent opens while
//!   the tmp file is live but does not tighten the DACL — the file
//!   still inherits the parent directory's ACL. A proper DACL-restrict
//!   step would need the `windows` crate, which is not a direct
//!   workspace dep today. Tracked as a TODO below.

use crate::error::AuthError;
pub use aura_core_auth::StoredSession;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Keyring service identifier used for every credential entry.
const KEYRING_SERVICE: &str = "aura";
/// Keyring username/slot under the service.
const KEYRING_USER: &str = "credentials";

/// Credential store backed by the OS keyring with a file fallback.
pub struct CredentialStore;

impl CredentialStore {
    /// Save a session in the OS credential store.
    ///
    /// On a [`keyring::Error::NoStorageAccess`] (headless Linux, CI images
    /// without a secret-service daemon) we downgrade to the 0600
    /// credentials file and emit a WARN log so the operator can see the
    /// fallback.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CredentialKeyring`] on unexpected keyring
    /// failures, [`AuthError::NoHomeDir`] when the fallback path cannot be
    /// resolved, or [`AuthError::CredentialIo`] on filesystem failures.
    pub fn save(session: &StoredSession) -> Result<(), AuthError> {
        let json = serde_json::to_string(session)?;

        match keyring_entry().and_then(|e| e.set_password(&json)) {
            Ok(()) => {
                debug!("Credentials saved to OS keyring");
                Ok(())
            }
            Err(e) if is_no_storage_access(&e) => {
                let path = credentials_path()?;
                warn!(
                    error = %e,
                    path = %path.display(),
                    "OS keyring unavailable; falling back to on-disk credentials file. \
                     This is a DEGRADED MODE — the JWT is written in plaintext. \
                     Install a secret-service daemon (gnome-keyring / KWallet) to re-enable keyring storage."
                );
                save_to_path(&path, &json)
            }
            Err(e) => Err(AuthError::CredentialKeyring(e.to_string())),
        }
    }

    /// Load the stored session, if any.
    ///
    /// Tries the OS keyring first; on `NoStorageAccess` OR any
    /// `NoEntry`-flavoured failure we probe the file path so existing
    /// users are not logged out during the keyring rollout.
    pub fn load() -> Option<StoredSession> {
        match keyring_entry().and_then(|e| e.get_password()) {
            Ok(json) => parse_session(&json, "keyring"),
            Err(e) if is_no_entry(&e) => load_from_default_path(),
            Err(e) if is_no_storage_access(&e) => {
                warn!(error = %e, "OS keyring unavailable; reading credentials from file fallback");
                load_from_default_path()
            }
            Err(e) => {
                warn!(error = %e, "Failed to read from OS keyring; trying file fallback");
                load_from_default_path()
            }
        }
    }

    /// Convenience: load only the JWT access token.
    #[must_use]
    pub fn load_token() -> Option<String> {
        Self::load().map(|s| s.access_token)
    }

    /// Delete the credentials from both the keyring and the fallback file.
    ///
    /// Succeeds silently when neither backing store has an entry.
    ///
    /// # Errors
    ///
    /// Returns [`AuthError::CredentialKeyring`] only on unexpected keyring
    /// failures (storage access / generic errors are logged + ignored).
    pub fn clear() -> Result<(), AuthError> {
        match keyring_entry().and_then(|e| e.delete_credential()) {
            Ok(()) => debug!("Credentials cleared from OS keyring"),
            Err(e) if is_no_entry(&e) || is_no_storage_access(&e) => {
                debug!(error = %e, "Keyring clear: no entry or no storage");
            }
            Err(e) => warn!(error = %e, "Keyring clear failed; continuing with file fallback"),
        }

        let path = credentials_path()?;
        match std::fs::remove_file(&path) {
            Ok(()) => debug!(?path, "Credentials file cleared"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AuthError::CredentialIo { path, source: e }),
        }
        // Best-effort: nuke any leftover tmp file from a crashed write.
        let tmp = tmp_path_for(&path);
        let _ = std::fs::remove_file(&tmp);
        Ok(())
    }
}

/// Build a keyring entry handle for the fixed (service, user) tuple.
fn keyring_entry() -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
}

/// Detect "no storage backend is available" errors — these are the ones
/// that should trigger the file fallback (headless Linux, locked-down
/// sandboxes, etc.). Other keyring errors are surfaced to the caller.
fn is_no_storage_access(e: &keyring::Error) -> bool {
    matches!(e, keyring::Error::NoStorageAccess(_))
}

/// Detect "no such entry" errors so `load`/`clear` can fall through to
/// the file backend rather than reporting a hard error on fresh installs.
fn is_no_entry(e: &keyring::Error) -> bool {
    matches!(e, keyring::Error::NoEntry)
}

fn parse_session(raw: &str, source: &str) -> Option<StoredSession> {
    match serde_json::from_str::<StoredSession>(raw) {
        Ok(session) => {
            debug!(%source, user_id = %session.user_id, "Loaded stored credentials");
            Some(session)
        }
        Err(e) => {
            warn!(%source, error = %e, "Stored credentials have invalid format");
            None
        }
    }
}

/// Resolve the credentials file path (`~/.aura/credentials.json`).
fn credentials_path() -> Result<PathBuf, AuthError> {
    dirs::home_dir()
        .map(|h| h.join(".aura").join("credentials.json"))
        .ok_or(AuthError::NoHomeDir)
}

/// Compute the sibling tmp-file path used by [`atomic_write_private`].
///
/// We keep the tmp file next to the final path (same directory, same
/// filesystem) so `rename` is a true atomic swap on both POSIX and NTFS.
fn tmp_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Save the JSON blob to `path` via an atomic tmp+rename dance.
fn save_to_path(path: &Path, json: &str) -> Result<(), AuthError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| AuthError::CredentialIo {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }

    atomic_write_private(path, json.as_bytes()).map_err(|e| AuthError::CredentialIo {
        path: path.to_path_buf(),
        source: e,
    })?;

    debug!(?path, "Credentials saved to file fallback (atomic)");
    Ok(())
}

fn load_from_default_path() -> Option<StoredSession> {
    let path = credentials_path().ok()?;
    load_from_path(&path)
}

fn load_from_path(path: &Path) -> Option<StoredSession> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(?path, error = %e, "Failed to read credentials file");
            return None;
        }
    };
    parse_session(&data, "file")
}

/// Atomically write `bytes` to `path` with restrictive permissions.
///
/// Writes to `<path>.tmp` first (creating it with platform-specific
/// private-file flags), fsyncs the data, then `rename`s the tmp file
/// over the final path. Both POSIX and NTFS guarantee `rename` within
/// a single directory is atomic: a concurrent reader sees either the
/// pre-write file or the post-write file, never a truncated prefix.
///
/// On Unix the tmp file is created `0o600` so even mid-write there is
/// no window during which group/other can read the JWT.
///
/// On Windows the tmp file is opened with `share_mode(0)` which blocks
/// any concurrent `CreateFileW` for the duration of the write, giving
/// the same "no half-written reads" property. Tightening the DACL down
/// to the owning SID requires the `windows` crate (not a current
/// workspace dep) and is tracked as a follow-up — see module docs.
fn atomic_write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let tmp = tmp_path_for(path);
    // Clear any stale tmp from a previous crashed write so the
    // OpenOptions flags below apply to a fresh inode. `NotFound` is fine.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    {
        let mut file = open_private_for_write(&tmp)?;
        file.write_all(bytes)?;
        file.flush()?;
        // Fsync so the rename below can't expose a zero-length file
        // after a power loss on filesystems that reorder metadata.
        file.sync_all()?;
    }

    // rename is atomic on the same filesystem on both POSIX and NTFS
    // (Windows MoveFileExW performs a transacted replace for same-volume
    // targets). If rename fails, the previous file is untouched.
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Best-effort cleanup so we don't leave a dangling tmp file.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(unix)]
fn open_private_for_write(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn open_private_for_write(path: &Path) -> io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    // share_mode(0) = FILE_SHARE_NONE: no other handle may open this
    // file while ours is live. This doesn't tighten the DACL (TODO:
    // follow-up w/ `windows` crate to set an owner-only DACL) but it
    // does ensure no concurrent read/write race can expose a truncated
    // JWT during the brief window before rename.
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_private_for_write(path: &Path) -> io::Result<std::fs::File> {
    // Unknown target: fall back to plain create+truncate. The outer
    // atomic_write_private still gives us tmp+rename semantics.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    fn sample_session() -> StoredSession {
        StoredSession {
            access_token: "tok_abc".to_string(),
            user_id: "user-1".to_string(),
            display_name: "Alice".to_string(),
            primary_zid: "0://alice".to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_stored_session_round_trip() {
        let session = sample_session();
        let json = serde_json::to_string(&session).unwrap();
        let restored: StoredSession = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.access_token, session.access_token);
        assert_eq!(restored.user_id, session.user_id);
        assert_eq!(restored.display_name, session.display_name);
        assert_eq!(restored.primary_zid, session.primary_zid);
    }

    #[test]
    fn test_parse_session_malformed_returns_none() {
        assert!(parse_session("not json at all", "test").is_none());
    }

    #[test]
    fn test_parse_session_valid_returns_some() {
        let raw = r#"{
            "access_token":"tok",
            "user_id":"u",
            "display_name":"n",
            "primary_zid":"z",
            "created_at":"2025-01-01T00:00:00Z"
        }"#;
        let parsed = parse_session(raw, "test").unwrap();
        assert_eq!(parsed.access_token, "tok");
    }

    #[test]
    fn test_tmp_path_for_appends_suffix() {
        let p = Path::new("/a/b/credentials.json");
        assert_eq!(tmp_path_for(p), Path::new("/a/b/credentials.json.tmp"));
    }

    #[test]
    fn test_atomic_write_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("creds.json");
        atomic_write_private(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, b"old contents that are longer than new").unwrap();
        atomic_write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn test_atomic_write_cleans_up_tmp_on_success() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("creds.json");
        atomic_write_private(&path, b"payload").unwrap();
        let tmp = tmp_path_for(&path);
        assert!(
            !tmp.exists(),
            "tmp file should be renamed away on success, but {tmp:?} still exists"
        );
    }

    #[test]
    fn test_atomic_write_clobbers_stale_tmp() {
        // A previous crashed write can leave <path>.tmp behind.
        // atomic_write_private must clobber it, not fail.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("creds.json");
        let tmp = tmp_path_for(&path);
        std::fs::write(&tmp, b"stale leftover from prior crash").unwrap();
        atomic_write_private(&path, b"fresh").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        assert!(!tmp.exists());
    }

    #[test]
    fn test_save_and_load_from_path_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("credentials.json");
        let session = sample_session();
        let json = serde_json::to_string(&session).unwrap();

        save_to_path(&path, &json).unwrap();
        let loaded = load_from_path(&path).expect("session should load");
        assert_eq!(loaded.access_token, session.access_token);
        assert_eq!(loaded.user_id, session.user_id);
        assert_eq!(loaded.primary_zid, session.primary_zid);
    }

    #[test]
    fn test_load_from_path_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(load_from_path(&dir.path().join("nope.json")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_perms_on_final_file_are_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("creds.json");
        atomic_write_private(&path, b"x").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "final file should be 0600, got {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn test_unix_perms_on_tmp_file_are_0600() {
        // We can't observe the tmp file after rename, so we stub in a
        // pre-existing tmp and check its mode right after the
        // inner open_private_for_write call by invoking it directly.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let tmp = dir.path().join("creds.json.tmp");
        {
            let _f = open_private_for_write(&tmp).unwrap();
        }
        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "tmp file should be 0600, got {mode:o}");
    }
}
