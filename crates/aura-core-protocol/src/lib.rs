//! # aura-core-protocol
//!
//! Layer: core
//!
//! Wire protocol primitives. Phase 1 ships only [`ProtocolVersion`];
//! the rich wire shapes currently live in `aura-protocol` and migrate
//! here in a later phase along with the surface-layer split.
//!
//! ## Invariants
//!
//! - Major bumps to [`ProtocolVersion`] require a migration shim in
//!   the consuming crate; minor bumps add fields with defaults.
//!
//! ## Failure modes
//!
//! - None at this layer — version negotiation lives in the surface
//!   crates.

#![forbid(unsafe_code)]
#![warn(clippy::all)]

use serde::{Deserialize, Serialize};

/// The current wire protocol version.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 1, minor: 0 };

/// Wire protocol version (major, minor).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    /// Breaking-change major number.
    pub major: u16,
    /// Backward-compatible minor number.
    pub minor: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_constant_is_one_zero() {
        assert_eq!(PROTOCOL_VERSION.major, 1);
        assert_eq!(PROTOCOL_VERSION.minor, 0);
    }

    #[test]
    fn version_roundtrips() {
        let v = ProtocolVersion { major: 2, minor: 5 };
        let json = serde_json::to_string(&v).unwrap();
        let back: ProtocolVersion = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
    }
}
