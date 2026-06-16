//! Sealed (encrypted-at-rest) value storage for the agent state DB.
//!
//! Swarm TEE upgrade phase 5 ("attest-boot" / sealed overlay): when the
//! harness runs inside a confidential VM with `AURA_STATE_ENCRYPTION=sealed`,
//! every *content-bearing* value persisted to RocksDB (record entries, inbox
//! transactions, memory facts/events/procedures, skill installations, tool
//! defaults, runtime-capability snapshots) is encrypted with AES-256-GCM
//! under a per-agent data encryption key (DEK) released by the Trustee KBS
//! after attestation.
//!
//! # Sealed value format (version 1)
//!
//! ```text
//! +----------------+---------+------------+--------------------------+
//! | magic (8)      | ver (1) | nonce (12) | ciphertext || tag (16)   |
//! | "AURASEAL"     | 0x01    | random     | AES-256-GCM              |
//! +----------------+---------+------------+--------------------------+
//! ```
//!
//! Each value gets a fresh random 96-bit nonce from the OS CSPRNG; with a
//! 256-bit key, random nonces are safe for far more values than a single
//! agent store will ever hold.
//!
//! # Scope / v1 tradeoffs (documented deliberately)
//!
//! * **Values only, keys pass through.** RocksDB keys are structural
//!   (agent id, sequence number, fact id, skill name) and stay plaintext so
//!   ordered iteration / prefix scans keep working. This is the DB analogue
//!   of "filename passthrough" in a sealed filesystem: an attacker with the
//!   ciphertext learns *shape* (how many entries, which agents), not content.
//! * **Pure-counter metadata stays plaintext.** Head/tail sequence cursors,
//!   the agent status byte, processing claims and the event-id → timestamp
//!   index carry no user content and are required before any value is
//!   decrypted.
//! * **Live-DB friendly.** Encrypting whole RocksDB SST/WAL files on
//!   open/close is not viable for a live database; sealing at the value
//!   layer keeps compaction, iterators and atomic `WriteBatch` semantics
//!   untouched.
//!
//! Plaintext mode (no cipher) is byte-for-byte identical to the historical
//! on-disk format — legacy agents are untouched.

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};
use zeroize::Zeroizing;

/// Magic prefix identifying a sealed value.
pub const SEAL_MAGIC: &[u8; 8] = b"AURASEAL";

/// Current sealed-value format version.
pub const SEAL_VERSION: u8 = 1;

/// Header length: magic (8) + version (1) + nonce (12).
const HEADER_LEN: usize = 8 + 1 + 12;

/// AES-GCM authentication tag length.
const TAG_LEN: usize = 16;

/// Errors produced by [`SealCipher`].
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// Encryption failed (should not happen with a valid key/nonce).
    #[error("failed to seal value")]
    Seal,
    /// The value is too short or does not carry the sealed magic/version.
    #[error("value is not in the sealed format (missing/invalid header)")]
    NotSealed,
    /// The sealed format version is newer than this build understands.
    #[error("unsupported sealed format version {0}")]
    UnsupportedVersion(u8),
    /// Authenticated decryption failed (wrong DEK or tampered ciphertext).
    #[error("failed to open sealed value (wrong key or tampered data)")]
    Open,
}

/// AES-256-GCM cipher sealing/opening individual store values.
///
/// Deliberately no `Debug`/`Clone` derive exposing internals: the wrapped
/// key schedule is the per-agent DEK. The key bytes handed to [`new`] should
/// be zeroized by the caller (e.g. held in [`Zeroizing`]); the internal key
/// schedule is zeroized on drop via the `aes`/`zeroize` integration.
///
/// [`new`]: SealCipher::new
pub struct SealCipher {
    aead: Aes256Gcm,
}

impl SealCipher {
    /// Build a cipher from a raw 256-bit DEK.
    #[must_use]
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)),
        }
    }

    /// Generate a fresh random 256-bit key from the OS CSPRNG.
    ///
    /// Used by dev-mode key-file provisioning; production DEKs come from
    /// the KBS. The returned buffer zeroizes on drop.
    #[must_use]
    pub fn generate_key() -> Zeroizing<[u8; 32]> {
        let key = Aes256Gcm::generate_key(OsRng);
        let mut out = Zeroizing::new([0u8; 32]);
        out.copy_from_slice(&key);
        out
    }

    /// Check whether `bytes` carries the sealed-value magic.
    #[must_use]
    pub fn is_sealed(bytes: &[u8]) -> bool {
        bytes.len() >= SEAL_MAGIC.len() && &bytes[..SEAL_MAGIC.len()] == SEAL_MAGIC
    }

    /// Seal `plaintext` into the versioned envelope with a fresh nonce.
    ///
    /// # Errors
    /// Returns [`SealError::Seal`] if AEAD encryption fails.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, SealError> {
        let nonce = Aes256Gcm::generate_nonce(OsRng);
        let ciphertext = self
            .aead
            .encrypt(&nonce, plaintext)
            .map_err(|_| SealError::Seal)?;

        let mut out = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        out.extend_from_slice(SEAL_MAGIC);
        out.push(SEAL_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Open a sealed envelope produced by [`seal`](Self::seal).
    ///
    /// # Errors
    /// * [`SealError::NotSealed`] — missing magic or truncated header.
    /// * [`SealError::UnsupportedVersion`] — version byte from the future.
    /// * [`SealError::Open`] — authentication failure (wrong key / tamper).
    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SealError> {
        if sealed.len() < HEADER_LEN + TAG_LEN || !Self::is_sealed(sealed) {
            return Err(SealError::NotSealed);
        }
        let version = sealed[SEAL_MAGIC.len()];
        if version != SEAL_VERSION {
            return Err(SealError::UnsupportedVersion(version));
        }
        let nonce = Nonce::from_slice(&sealed[SEAL_MAGIC.len() + 1..HEADER_LEN]);
        self.aead
            .decrypt(nonce, &sealed[HEADER_LEN..])
            .map_err(|_| SealError::Open)
    }
}

impl std::fmt::Debug for SealCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose key material; the cipher is identified by format only.
        f.debug_struct("SealCipher")
            .field("cipher", &"aes-256-gcm")
            .field("version", &SEAL_VERSION)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cipher() -> SealCipher {
        SealCipher::new(&[7u8; 32])
    }

    #[test]
    fn seal_open_roundtrip() {
        let cipher = test_cipher();
        let plain = br#"{"kind":"record","data":"hello sealed world"}"#;
        let sealed = cipher.seal(plain).unwrap();
        assert_ne!(sealed.as_slice(), plain.as_slice());
        let opened = cipher.open(&sealed).unwrap();
        assert_eq!(opened, plain);
    }

    #[test]
    fn sealed_envelope_has_magic_version_and_no_plaintext() {
        let cipher = test_cipher();
        let plain = b"super secret agent state";
        let sealed = cipher.seal(plain).unwrap();

        assert_eq!(&sealed[..8], SEAL_MAGIC);
        assert_eq!(sealed[8], SEAL_VERSION);
        assert!(SealCipher::is_sealed(&sealed));
        // Ciphertext must not contain the plaintext anywhere.
        assert!(!sealed.windows(plain.len()).any(|w| w == plain.as_slice()));
    }

    #[test]
    fn fresh_nonce_per_seal() {
        let cipher = test_cipher();
        let a = cipher.seal(b"same plaintext").unwrap();
        let b = cipher.seal(b"same plaintext").unwrap();
        assert_ne!(
            a, b,
            "two seals of the same plaintext must differ (random nonce)"
        );
    }

    #[test]
    fn tampered_ciphertext_fails_open() {
        let cipher = test_cipher();
        let mut sealed = cipher.seal(b"payload").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(matches!(cipher.open(&sealed), Err(SealError::Open)));
    }

    #[test]
    fn wrong_key_fails_open() {
        let sealed = test_cipher().seal(b"payload").unwrap();
        let other = SealCipher::new(&[8u8; 32]);
        assert!(matches!(other.open(&sealed), Err(SealError::Open)));
    }

    #[test]
    fn plaintext_input_is_rejected() {
        let cipher = test_cipher();
        assert!(matches!(
            cipher.open(br#"{"plain":"json"}"#),
            Err(SealError::NotSealed)
        ));
        assert!(matches!(cipher.open(b""), Err(SealError::NotSealed)));
        assert!(!SealCipher::is_sealed(b"{\"plain\":true}"));
    }

    #[test]
    fn future_version_is_rejected() {
        let cipher = test_cipher();
        let mut sealed = cipher.seal(b"payload").unwrap();
        sealed[8] = 99;
        assert!(matches!(
            cipher.open(&sealed),
            Err(SealError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn generated_keys_are_random_and_usable() {
        let k1 = SealCipher::generate_key();
        let k2 = SealCipher::generate_key();
        assert_ne!(*k1, *k2);

        let cipher = SealCipher::new(&k1);
        let sealed = cipher.seal(b"dev mode").unwrap();
        assert_eq!(cipher.open(&sealed).unwrap(), b"dev mode");
    }

    #[test]
    fn debug_does_not_leak_key() {
        let rendered = format!("{:?}", test_cipher());
        assert!(rendered.contains("aes-256-gcm"));
        assert!(
            !rendered.contains('7'),
            "debug output must not include key bytes: {rendered}"
        );
    }
}
