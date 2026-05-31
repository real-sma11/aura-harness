//! Hashing utilities for the Aura system.
//!
//! Uses BLAKE3 for all hashing operations.

#[allow(deprecated)]
use crate::ids::TxId;

/// Hash arbitrary bytes using BLAKE3.
#[must_use]
pub fn hash_bytes(data: &[u8]) -> [u8; 32] {
    *blake3::hash(data).as_bytes()
}

/// Hash multiple byte slices together.
#[must_use]
pub fn hash_many(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(part);
    }
    *hasher.finalize().as_bytes()
}

/// Compute a context hash from transaction bytes and record window.
///
/// This is used to create a deterministic fingerprint of the inputs
/// used to make a decision.
#[must_use]
pub fn compute_context_hash(tx_bytes: &[u8], record_window_bytes: &[u8]) -> [u8; 32] {
    hash_many(&[tx_bytes, record_window_bytes])
}

/// Generate a transaction ID from its content.
#[deprecated(note = "use Hash::from_content — tx_id_from_content is a legacy alias")]
#[allow(deprecated)]
#[must_use]
pub fn tx_id_from_content(content: &[u8]) -> TxId {
    TxId::new(hash_bytes(content))
}

/// Incremental hasher for building hashes from multiple parts.
pub struct Hasher {
    inner: blake3::Hasher,
}

impl Hasher {
    /// Create a new hasher.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: blake3::Hasher::new(),
        }
    }

    /// Update the hasher with more data.
    pub fn update(&mut self, data: &[u8]) -> &mut Self {
        self.inner.update(data);
        self
    }

    /// Finalize and return the hash.
    #[must_use]
    pub fn finalize(self) -> [u8; 32] {
        *self.inner.finalize().as_bytes()
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Hash;

    #[test]
    fn hash_bytes_deterministic() {
        let data = b"test data";
        let hash1 = hash_bytes(data);
        let hash2 = hash_bytes(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_bytes_different_input() {
        let hash1 = hash_bytes(b"data1");
        let hash2 = hash_bytes(b"data2");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn hash_many_order_matters() {
        let hash1 = hash_many(&[b"part1", b"part2"]);
        let hash2 = hash_many(&[b"part2", b"part1"]);
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn incremental_hasher() {
        let direct = hash_many(&[b"part1", b"part2"]);

        let mut hasher = Hasher::new();
        hasher.update(b"part1").update(b"part2");
        let incremental = hasher.finalize();

        assert_eq!(direct, incremental);
    }

    #[test]
    fn context_hash_deterministic() {
        let tx = b"transaction data";
        let window = b"record window data";

        let hash1 = compute_context_hash(tx, window);
        let hash2 = compute_context_hash(tx, window);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn hash_chained_empty_inputs() {
        let h1 = Hash::from_content_chained(b"", None);
        let h2 = Hash::from_content_chained(b"", None);
        assert_eq!(h1, h2);

        let h3 = Hash::from_content(b"");
        assert_eq!(h1, h3);
    }

    #[test]
    fn hash_chained_empty_with_prev() {
        let prev = Hash::from_content(b"prev");
        let h1 = Hash::from_content_chained(b"", Some(&prev));
        let h2 = Hash::from_content_chained(b"", None);
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_chained_order_matters() {
        let a = Hash::from_content(b"A");
        let b = Hash::from_content(b"B");

        let h_ab = Hash::from_content_chained(a.as_bytes(), Some(&b));
        let h_ba = Hash::from_content_chained(b.as_bytes(), Some(&a));

        assert_ne!(h_ab, h_ba);
    }

    #[test]
    fn hash_chained_max_size_input() {
        let large_input = vec![0xFF; 65536];
        let h1 = Hash::from_content_chained(&large_input, None);
        let h2 = Hash::from_content_chained(&large_input, None);
        assert_eq!(h1, h2);

        let large_input_2 = vec![0xFE; 65536];
        let h3 = Hash::from_content_chained(&large_input_2, None);
        assert_ne!(h1, h3);
    }

    #[test]
    fn hash_bytes_empty_input() {
        let h = hash_bytes(b"");
        assert_ne!(h, [0u8; 32]);
    }

    #[test]
    fn hash_many_empty_parts() {
        let h1 = hash_many(&[]);
        let h2 = hash_many(&[b""]);
        let h3 = hash_many(&[b"", b""]);
        assert_eq!(h1, h2);
        assert_eq!(h2, h3);
    }

    #[test]
    fn hash_many_single_vs_concatenated() {
        let h1 = hash_many(&[b"helloworld"]);
        let h2 = hash_many(&[b"hello", b"world"]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_many_order_matters_three_parts() {
        let h1 = hash_many(&[b"a", b"b", b"c"]);
        let h2 = hash_many(&[b"c", b"b", b"a"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn context_hash_different_inputs() {
        let h1 = compute_context_hash(b"tx1", b"window1");
        let h2 = compute_context_hash(b"tx2", b"window1");
        let h3 = compute_context_hash(b"tx1", b"window2");

        assert_ne!(h1, h2);
        assert_ne!(h1, h3);
        assert_ne!(h2, h3);
    }

    #[allow(deprecated)]
    #[test]
    fn tx_id_from_content_matches_hash() {
        let content = b"test content";
        let tx_id = tx_id_from_content(content);
        let hash = hash_bytes(content);
        assert_eq!(*tx_id.as_bytes(), hash);
    }

    #[test]
    fn incremental_hasher_default() {
        let h1 = Hasher::default().finalize();
        let h2 = Hasher::new().finalize();
        assert_eq!(h1, h2);
    }
}
