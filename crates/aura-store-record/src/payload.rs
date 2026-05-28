//! `RecordPayload` — audited-payload representation.
//!
//! Phase 6a populates the [`RecordPayload::Summary`] variant for
//! `KernelMode::AuditedLite`: a fixed-size head/tail window of the
//! original bytes plus a content hash + full length so audit, billing,
//! and replay flows still see the metadata they need without paying
//! for the entire payload on disk. The full payload is intentionally
//! NOT persisted in the record log; replay/audit flows that need the
//! original bytes go to `aura-store-snapshot` (a no-op stub until
//! Phase 6b). Phase 2 shipped the `Inline`-only stub.
//!
//! ## Wire format
//!
//! Externally tagged via `#[serde(rename_all = "snake_case")]`:
//!
//! - `{"inline": [..bytes..]}` for the full-fidelity tier.
//! - `{"summary": { "head": [..], "tail": [..], "full_hash": "..", "full_len": N }}`
//!   for the summary tier.
//!
//! Internal tagging (`#[serde(tag = "kind")]`) is incompatible with
//! serde's tuple variants and complicates the struct variant, so the
//! payload uses external tagging while [`crate::RecordKind`] (whose
//! variants are all unit) keeps the more compact internal-tag form.
//! Both shapes coexist on the wire because the kernel writes them
//! into separate JSON fields of [`crate::RecordEntry`].
//!
//! ## Invariants (per `.cursor/rules.md` §13)
//!
//! - [`RecordPayload::Inline`] always carries the entire payload. It
//!   is the unconditional shape for `KernelMode::Audited` and for any
//!   record below the per-mode size threshold.
//! - [`RecordPayload::Summary`] is the AuditedLite shape: `head` is
//!   the first `chunk` bytes, `tail` is the last `chunk` bytes, and
//!   `full_hash` is the BLAKE3 hash of the COMPLETE original payload.
//!   `full_len` carries the original byte length.
//! - For payloads smaller than `2 * chunk` bytes the head/tail
//!   windows would overlap, so [`summarize_payload`] degenerates the
//!   chunk size to `bytes.len() / 2` (an exact non-overlapping split)
//!   to preserve the "head + tail are disjoint slices of the input"
//!   guarantee.
//!
//! ## Failure modes
//!
//! - None at the type level. Replay-time `SnapshotMissing` errors
//!   surface in Phase 6b when `aura-store-snapshot` gains a real
//!   backend.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// Default head/tail chunk size used by [`summarize_payload`] when
/// the caller does not supply an explicit override. Matches the
/// 1 KiB recommendation in the architecture plan §4.
pub const DEFAULT_SUMMARY_CHUNK_BYTES: usize = 1024;

/// Audited-payload representation.
///
/// `KernelMode::Audited` always produces [`RecordPayload::Inline`];
/// `KernelMode::AuditedLite` produces [`RecordPayload::Summary`]
/// when the payload exceeds the configured threshold and
/// [`RecordPayload::Inline`] for smaller payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordPayload {
    /// Full inline payload bytes. The unconditional shape for
    /// `KernelMode::Audited`.
    Inline(#[serde(with = "bytes_serde")] Bytes),
    /// Phase 6a summary shape for `KernelMode::AuditedLite`. Carries
    /// the first / last `chunk` bytes of the payload plus a content
    /// hash and original length so replay, billing, and audit flows
    /// can verify and resolve the full payload later via
    /// `aura-store-snapshot`.
    Summary {
        /// First `chunk` bytes of the payload (hex view in audit UIs).
        #[serde(with = "bytes_serde")]
        head: Bytes,
        /// Last `chunk` bytes of the payload (hex view in audit UIs).
        #[serde(with = "bytes_serde")]
        tail: Bytes,
        /// BLAKE3 hex digest of the COMPLETE original payload. Used
        /// to verify the snapshot store fetch in replay.
        full_hash: String,
        /// Original payload length in bytes.
        full_len: usize,
    },
}

impl RecordPayload {
    /// Construct an [`RecordPayload::Inline`] from any byte source.
    #[must_use]
    pub fn inline(bytes: impl Into<Bytes>) -> Self {
        Self::Inline(bytes.into())
    }
}

/// Summarise `bytes` into [`RecordPayload::Inline`] when below
/// `threshold`, or [`RecordPayload::Summary`] otherwise.
///
/// The summary carries `head = bytes[..chunk]`, `tail = bytes[len-chunk..]`,
/// `full_hash = blake3(bytes).to_hex()`, `full_len = bytes.len()`.
///
/// `chunk` defaults to [`DEFAULT_SUMMARY_CHUNK_BYTES`] (1 KiB) but
/// is reduced to `bytes.len() / 2` for payloads smaller than `2 *
/// DEFAULT_SUMMARY_CHUNK_BYTES` so the head/tail windows never
/// overlap.
#[must_use]
pub fn summarize_payload(bytes: &[u8], threshold: usize) -> RecordPayload {
    if bytes.len() <= threshold {
        return RecordPayload::Inline(Bytes::copy_from_slice(bytes));
    }
    let raw_chunk = DEFAULT_SUMMARY_CHUNK_BYTES;
    let chunk = raw_chunk.min(bytes.len() / 2);
    let head = Bytes::copy_from_slice(&bytes[..chunk]);
    let tail = Bytes::copy_from_slice(&bytes[bytes.len() - chunk..]);
    let full_hash = blake3::hash(bytes).to_hex().to_string();
    RecordPayload::Summary {
        head,
        tail,
        full_hash,
        full_len: bytes.len(),
    }
}

/// Serde adapter for `bytes::Bytes` that round-trips through a
/// `Vec<u8>`. Avoids pulling in `serde_bytes` for a single use site.
mod bytes_serde {
    use bytes::Bytes;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S: Serializer>(value: &Bytes, ser: S) -> Result<S::Ok, S::Error> {
        value.as_ref().to_vec().serialize(ser)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Bytes, D::Error> {
        let raw: Vec<u8> = Vec::deserialize(de)?;
        Ok(Bytes::from(raw))
    }
}
