//! New identifier newtypes for the layered architecture.
//!
//! # Invariants
//!
//! - Fixed-size byte layouts so hex display widths are stable.
//! - `Hash`/`Display`/`Debug` mask long ids to the first 16 hex chars
//!   to keep logs readable.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! define_id_16 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub [u8; 16]);

        impl $name {
            /// Construct from raw bytes.
            #[must_use]
            pub const fn new(bytes: [u8; 16]) -> Self { Self(bytes) }

            /// Borrow the inner byte array.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] { &self.0 }

            /// Hex encoding.
            #[must_use]
            pub fn to_hex(&self) -> String { hex::encode(self.0) }

            /// Parse from a hex string.
            ///
            /// # Errors
            ///
            /// Returns [`hex::FromHexError`] when the input is not a
            /// 32-character hex string.
            pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
                let bytes = hex::decode(s)?;
                let arr: [u8; 16] = bytes
                    .try_into()
                    .map_err(|_| hex::FromHexError::InvalidStringLength)?;
                Ok(Self(arr))
            }

            /// Generate a random id.
            #[must_use]
            pub fn generate() -> Self { Self(*uuid::Uuid::new_v4().as_bytes()) }

            /// Derive an id from a UUID.
            #[must_use]
            pub fn from_uuid(uuid: uuid::Uuid) -> Self { Self(*uuid.as_bytes()) }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.to_hex())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.to_hex())
            }
        }
    };
}

macro_rules! define_id_32 {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub [u8; 32]);

        impl $name {
            /// Construct from raw bytes.
            #[must_use]
            pub const fn new(bytes: [u8; 32]) -> Self { Self(bytes) }

            /// Borrow the inner byte array.
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] { &self.0 }

            /// Hex encoding.
            #[must_use]
            pub fn to_hex(&self) -> String { hex::encode(self.0) }

            /// Parse from a hex string.
            ///
            /// # Errors
            ///
            /// Returns [`hex::FromHexError`] when the input is not a
            /// 64-character hex string.
            pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
                let bytes = hex::decode(s)?;
                let arr: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| hex::FromHexError::InvalidStringLength)?;
                Ok(Self(arr))
            }

            /// Derive an id by hashing a UUID's bytes with blake3.
            #[must_use]
            pub fn from_uuid(uuid: uuid::Uuid) -> Self {
                Self(*blake3::hash(uuid.as_bytes()).as_bytes())
            }

            /// Generate a random id.
            #[must_use]
            pub fn generate() -> Self { Self::from_uuid(uuid::Uuid::new_v4()) }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let hex = self.to_hex();
                let trunc = if hex.len() > 16 { &hex[..16] } else { &hex };
                write!(f, "{}({})", stringify!($name), trunc)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let hex = self.to_hex();
                if hex.len() > 16 { write!(f, "{}", &hex[..16]) } else { write!(f, "{hex}") }
            }
        }
    };
}

define_id_16! {
    /// Identifier for an agent turn — one model→tool→model round.
    TurnId
}

define_id_16! {
    /// Identifier for a logical session "run" (multiple turns).
    RunId
}

define_id_16! {
    /// Identifier minted by the model layer for every tool call.
    ToolCallId
}

define_id_16! {
    /// Opaque end-user identifier.
    UserId
}

define_id_16! {
    /// Surface session identifier (one per connected client).
    SessionId
}

define_id_32! {
    /// Content-addressed transaction id (legacy `Hash`-shaped).
    TransactionId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_hex() {
        let id = TurnId::generate();
        let hex = id.to_hex();
        let back = TurnId::from_hex(&hex).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn ids_have_stable_serde() {
        let id = SessionId::new([7u8; 16]);
        let json = serde_json::to_string(&id).unwrap();
        let back: SessionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn transaction_id_hashes_uuid_bytes() {
        let uuid = uuid::Uuid::new_v4();
        let id = TransactionId::from_uuid(uuid);
        assert_ne!(id, TransactionId::default());
    }
}

// ============================================================================
// Legacy identifiers (migrated from `aura-core`).
//
// These predate the `define_id_16!`/`define_id_32!` newtypes above and use a
// separate `define_id!` macro that wires hex serde via the `serde_helpers`
// module. They are preserved verbatim so downstream call sites keep compiling.
// ============================================================================

macro_rules! define_id {
    (
        $(#[$meta:meta])*
        $name:ident, $len:expr, $serde_mod:expr, truncate = $trunc:expr
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(#[serde(with = $serde_mod)] pub [u8; $len]);

        #[allow(deprecated)]
        impl $name {
            #[must_use]
            pub const fn new(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            #[must_use]
            pub fn to_hex(&self) -> String {
                hex::encode(self.0)
            }

            /// # Errors
            /// Returns error if hex string is invalid or wrong length.
            pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
                let bytes = hex::decode(s)?;
                let arr: [u8; $len] = bytes
                    .try_into()
                    .map_err(|_| hex::FromHexError::InvalidStringLength)?;
                Ok(Self(arr))
            }
        }

        #[allow(deprecated)]
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let hex = self.to_hex();
                let display = if $trunc > 0 && hex.len() > $trunc {
                    &hex[..$trunc]
                } else {
                    &hex
                };
                write!(f, "{}({})", stringify!($name), display)
            }
        }

        #[allow(deprecated)]
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let hex = self.to_hex();
                if $trunc > 0 && hex.len() > $trunc {
                    write!(f, "{}", &hex[..$trunc])
                } else {
                    write!(f, "{}", hex)
                }
            }
        }
    };
}

// ============================================================================
// Hash Type (32 bytes, blake3)
// ============================================================================

define_id!(
    /// A 32-byte blake3 hash used for transaction chaining.
    Hash, 32, "crate::serde_helpers::hex_bytes_32", truncate = 16
);

impl Hash {
    /// Create hash from content only (genesis transaction).
    #[must_use]
    pub fn from_content(content: &[u8]) -> Self {
        let hash = blake3::hash(content);
        Self(*hash.as_bytes())
    }

    /// Create hash from content and previous transaction's hash.
    /// Genesis transaction passes `None` for `prev_hash`.
    #[must_use]
    pub fn from_content_chained(content: &[u8], prev_hash: Option<&Self>) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(content);
        if let Some(prev) = prev_hash {
            hasher.update(&prev.0);
        }
        Self(*hasher.finalize().as_bytes())
    }
}

// ============================================================================
// Agent ID (32 bytes)
// ============================================================================

define_id!(
    /// Agent identifier - 32 bytes, derived from identity hash or UUID.
    AgentId, 32, "crate::serde_helpers::hex_bytes_32", truncate = 16
);

impl AgentId {
    /// Create an `AgentId` from a UUID v4.
    #[must_use]
    pub fn from_uuid(uuid: uuid::Uuid) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(uuid.as_bytes());
        let hash = hasher.finalize();
        Self(*hash.as_bytes())
    }

    /// Generate a new random `AgentId`.
    #[must_use]
    pub fn generate() -> Self {
        Self::from_uuid(uuid::Uuid::new_v4())
    }
}

// ============================================================================
// Transaction ID (32 bytes)
// ============================================================================

define_id!(
    #[deprecated(note = "use Hash — TxId is a legacy alias")]
    /// Transaction identifier - 32 bytes, typically a hash of tx content.
    TxId,
    32,
    "crate::serde_helpers::hex_bytes_32",
    truncate = 16
);

#[allow(deprecated)]
impl TxId {
    /// Generate a `TxId` by hashing content.
    #[must_use]
    pub fn from_content(content: &[u8]) -> Self {
        let hash = blake3::hash(content);
        Self(*hash.as_bytes())
    }
}

// ============================================================================
// Action ID (16 bytes)
// ============================================================================

define_id!(
    /// Action identifier - 16 bytes, generated per action.
    ActionId, 16, "crate::serde_helpers::hex_bytes_16", truncate = 0
);

impl ActionId {
    /// Generate a new random `ActionId`.
    #[must_use]
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self(*uuid.as_bytes())
    }
}

// ============================================================================
// Process ID (16 bytes)
// ============================================================================

define_id!(
    /// Process identifier - 16 bytes, generated per async process.
    ProcessId, 16, "crate::serde_helpers::hex_bytes_16", truncate = 0
);

impl ProcessId {
    /// Generate a new random `ProcessId`.
    #[must_use]
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self(*uuid.as_bytes())
    }
}

// ============================================================================
// Fact ID (16 bytes)
// ============================================================================

define_id!(
    /// Fact identifier - 16 bytes.
    FactId, 16, "crate::serde_helpers::hex_bytes_16", truncate = 0
);

impl FactId {
    /// Generate a new random `FactId`.
    #[must_use]
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self(*uuid.as_bytes())
    }
}

// ============================================================================
// Agent Event ID (16 bytes)
// ============================================================================

define_id!(
    /// Agent event identifier - 16 bytes.
    AgentEventId, 16, "crate::serde_helpers::hex_bytes_16", truncate = 0
);

impl AgentEventId {
    /// Generate a new random `AgentEventId`.
    #[must_use]
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self(*uuid.as_bytes())
    }
}

// ============================================================================
// Procedure ID (16 bytes)
// ============================================================================

define_id!(
    /// Procedure identifier - 16 bytes.
    ProcedureId, 16, "crate::serde_helpers::hex_bytes_16", truncate = 0
);

impl ProcedureId {
    /// Generate a new random `ProcedureId`.
    #[must_use]
    pub fn generate() -> Self {
        let uuid = uuid::Uuid::new_v4();
        Self(*uuid.as_bytes())
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod legacy_id_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn proptest_agent_id_different_inputs_produce_different_ids(
            a in any::<[u8; 16]>(),
            b in any::<[u8; 16]>(),
        ) {
            let uuid_a = uuid::Uuid::from_bytes(a);
            let uuid_b = uuid::Uuid::from_bytes(b);
            let id_a = AgentId::from_uuid(uuid_a);
            let id_b = AgentId::from_uuid(uuid_b);
            if a == b {
                prop_assert_eq!(id_a, id_b);
            } else {
                prop_assert_ne!(id_a, id_b);
            }
        }

        #[test]
        fn proptest_tx_id_different_content_produces_different_ids(
            a in proptest::collection::vec(any::<u8>(), 1..256),
            b in proptest::collection::vec(any::<u8>(), 1..256),
        ) {
            let id_a = TxId::from_content(&a);
            let id_b = TxId::from_content(&b);
            if a == b {
                prop_assert_eq!(id_a, id_b);
            } else {
                prop_assert_ne!(id_a, id_b);
            }
        }

        #[test]
        fn proptest_action_id_hex_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = ActionId::new(bytes);
            let hex = id.to_hex();
            let parsed = ActionId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }

        #[test]
        fn proptest_agent_id_hex_roundtrip(bytes in any::<[u8; 32]>()) {
            let id = AgentId::new(bytes);
            let hex = id.to_hex();
            let parsed = AgentId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }

        #[test]
        fn proptest_hash_hex_roundtrip(bytes in any::<[u8; 32]>()) {
            let hash = Hash::new(bytes);
            let hex = hash.to_hex();
            let parsed = Hash::from_hex(&hex).unwrap();
            prop_assert_eq!(hash, parsed);
        }

        #[test]
        fn proptest_process_id_hex_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = ProcessId::new(bytes);
            let hex = id.to_hex();
            let parsed = ProcessId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }
    }

    #[test]
    fn agent_id_generate_uniqueness() {
        let ids: Vec<AgentId> = (0..100).map(|_| AgentId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated IDs should be unique");
            }
        }
    }

    #[test]
    fn action_id_generate_uniqueness() {
        let ids: Vec<ActionId> = (0..100).map(|_| ActionId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated IDs should be unique");
            }
        }
    }

    #[test]
    fn process_id_generate_uniqueness() {
        let ids: Vec<ProcessId> = (0..100).map(|_| ProcessId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated IDs should be unique");
            }
        }
    }

    #[test]
    fn hash_from_hex_invalid_length() {
        assert!(Hash::from_hex("abcd").is_err());
        assert!(Hash::from_hex("").is_err());
    }

    #[test]
    fn hash_from_hex_invalid_chars() {
        let bad_hex = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        assert!(Hash::from_hex(bad_hex).is_err());
    }

    #[test]
    fn agent_id_display_and_debug() {
        let id = AgentId::new([0xAB; 32]);
        let display = format!("{id}");
        let debug = format!("{id:?}");
        assert!(display.len() == 16);
        assert!(debug.contains("AgentId("));
    }

    #[test]
    fn hash_display_and_debug() {
        let hash = Hash::from_content(b"test");
        let display = format!("{hash}");
        let debug = format!("{hash:?}");
        assert!(display.len() == 16);
        assert!(debug.contains("Hash("));
    }

    #[test]
    fn hash_genesis() {
        let content = b"genesis transaction";
        let hash1 = Hash::from_content(content);
        let hash2 = Hash::from_content(content);
        assert_eq!(hash1, hash2);

        let hash3 = Hash::from_content_chained(content, None);
        assert_eq!(hash1, hash3);
    }

    #[test]
    fn hash_chaining() {
        let content1 = b"first transaction";
        let content2 = b"second transaction";

        let hash1 = Hash::from_content(content1);
        let hash2 = Hash::from_content_chained(content2, Some(&hash1));

        let hash3 = Hash::from_content_chained(content2, None);
        assert_ne!(hash2, hash3);

        let hash4 = Hash::from_content_chained(content2, Some(&hash1));
        assert_eq!(hash2, hash4);
    }

    #[test]
    fn hash_chain_integrity() {
        let h1 = Hash::from_content(b"tx1");
        let h2 = Hash::from_content_chained(b"tx2", Some(&h1));
        let h3 = Hash::from_content_chained(b"tx3", Some(&h2));

        let h2_modified = Hash::from_content_chained(b"tx2-modified", Some(&h1));
        assert_ne!(h2, h2_modified);

        let h3_from_modified = Hash::from_content_chained(b"tx3", Some(&h2_modified));
        assert_ne!(h3, h3_from_modified);
    }

    #[test]
    fn hash_roundtrip() {
        let hash = Hash::from_content(b"test content");
        let hex = hash.to_hex();
        let parsed = Hash::from_hex(&hex).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn hash_json_roundtrip() {
        let hash = Hash::from_content(b"test content");
        let json = serde_json::to_string(&hash).unwrap();
        let parsed: Hash = serde_json::from_str(&json).unwrap();
        assert_eq!(hash, parsed);
    }

    #[test]
    fn agent_id_roundtrip() {
        let id = AgentId::generate();
        let hex = id.to_hex();
        let parsed = AgentId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_id_json_roundtrip() {
        let id = AgentId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: AgentId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn tx_id_from_content() {
        let content = b"test transaction content";
        let id1 = TxId::from_content(content);
        let id2 = TxId::from_content(content);
        assert_eq!(id1, id2);

        let id3 = TxId::from_content(b"different content");
        assert_ne!(id1, id3);
    }

    #[test]
    fn action_id_roundtrip() {
        let id = ActionId::generate();
        let hex = id.to_hex();
        let parsed = ActionId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn action_id_json_roundtrip() {
        let id = ActionId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: ActionId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn process_id_roundtrip() {
        let id = ProcessId::generate();
        let hex = id.to_hex();
        let parsed = ProcessId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn process_id_json_roundtrip() {
        let id = ProcessId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: ProcessId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn fact_id_hex_roundtrip() {
        let id = FactId::generate();
        let hex = id.to_hex();
        let parsed = FactId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn fact_id_json_roundtrip() {
        let id = FactId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: FactId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn fact_id_generate_uniqueness() {
        let ids: Vec<FactId> = (0..100).map(|_| FactId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated FactIds should be unique");
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_fact_id_hex_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = FactId::new(bytes);
            let hex = id.to_hex();
            let parsed = FactId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }
    }

    #[test]
    fn agent_event_id_hex_roundtrip() {
        let id = AgentEventId::generate();
        let hex = id.to_hex();
        let parsed = AgentEventId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_event_id_json_roundtrip() {
        let id = AgentEventId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: AgentEventId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn agent_event_id_generate_uniqueness() {
        let ids: Vec<AgentEventId> = (0..100).map(|_| AgentEventId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated AgentEventIds should be unique");
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_agent_event_id_hex_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = AgentEventId::new(bytes);
            let hex = id.to_hex();
            let parsed = AgentEventId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }
    }

    #[test]
    fn procedure_id_hex_roundtrip() {
        let id = ProcedureId::generate();
        let hex = id.to_hex();
        let parsed = ProcedureId::from_hex(&hex).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn procedure_id_json_roundtrip() {
        let id = ProcedureId::generate();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: ProcedureId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn procedure_id_generate_uniqueness() {
        let ids: Vec<ProcedureId> = (0..100).map(|_| ProcedureId::generate()).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Generated ProcedureIds should be unique");
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_procedure_id_hex_roundtrip(bytes in any::<[u8; 16]>()) {
            let id = ProcedureId::new(bytes);
            let hex = id.to_hex();
            let parsed = ProcedureId::from_hex(&hex).unwrap();
            prop_assert_eq!(id, parsed);
        }
    }
}
