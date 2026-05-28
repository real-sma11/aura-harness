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
