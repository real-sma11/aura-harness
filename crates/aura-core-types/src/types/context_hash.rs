//! `ContextHash` newtype — Invariant §6 context fingerprint.
//!
//! Wraps a 32-byte BLAKE3 digest so that record entries and streaming
//! handles can no longer pass raw arrays where a context hash is
//! expected.
//!
//! The serde representation is byte-identical to the previous
//! `serde(with = "hex_bytes_32")` field handling so existing
//! RocksDB payloads continue to deserialize.

use serde::{Deserialize, Serialize};

/// Deterministic context fingerprint for a [`crate::types::RecordEntry`].
///
/// See `docs/invariants.md` §6 for the derivation rule.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextHash(#[serde(with = "crate::serde_helpers::hex_bytes_32")] pub [u8; 32]);

impl ContextHash {
    /// Return the all-zero sentinel used by pre-canonical call sites.
    ///
    /// New code should never construct this value in production paths;
    /// it remains as a migration aid for tests and legacy builders.
    #[must_use]
    pub const fn zero() -> Self {
        Self([0u8; 32])
    }

    /// Hex representation of the full 32-byte digest.
    #[must_use]
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl AsRef<[u8; 32]> for ContextHash {
    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<[u8; 32]> for ContextHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl From<ContextHash> for [u8; 32] {
    fn from(hash: ContextHash) -> Self {
        hash.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_all_zeros() {
        assert_eq!(ContextHash::zero().0, [0u8; 32]);
    }

    #[test]
    fn as_hex_matches_hex_encode() {
        let bytes = [0xAB; 32];
        let ctx = ContextHash(bytes);
        assert_eq!(ctx.as_hex(), hex::encode(bytes));
    }

    #[test]
    fn serde_representation_matches_legacy_hex_bytes_32() {
        // The on-disk representation must stay byte-identical to the
        // previous `#[serde(with = "hex_bytes_32")]` field handling so
        // existing RocksDB payloads keep deserializing. Verify both
        // directions.
        let bytes = [0x11u8; 32];
        let ctx = ContextHash(bytes);
        let json = serde_json::to_string(&ctx).unwrap();
        // Legacy: hex-encoded string.
        assert_eq!(json, format!("\"{}\"", hex::encode(bytes)));

        let parsed: ContextHash = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ctx);
    }

    #[test]
    fn conversions_round_trip() {
        let bytes = [0x42u8; 32];
        let ctx: ContextHash = bytes.into();
        let back: [u8; 32] = ctx.into();
        assert_eq!(back, bytes);
        assert_eq!(ctx.as_ref(), &bytes);
    }
}
