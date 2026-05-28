//! Per-mode capability ceiling expressed as opaque discriminant strings.
//!
//! # Layering note
//!
//! `aura-core-modes` is a leaf crate (zero `aura-*` deps); we cannot
//! reference `aura-core-permissions::Capability` directly without
//! reversing the dependency. We instead express the per-mode ceiling
//! as a `BTreeSet<&'static str>` of capability discriminants. The
//! permissions crate maps these strings back to its richer
//! `Capability` enum when computing `EffectivePermissions`.
//!
//! The string set is intentionally small and only references
//! discriminants that map 1:1 to a `Capability` variant. The
//! discriminants are stable wire identifiers — changing them is a
//! breaking change for both crates.

use std::collections::BTreeSet;

/// The set of capability discriminants permitted by a given mode.
///
/// Forms the `mode` half of
/// `effective = mode ∩ user_grants`.
///
/// Not serialised — this is a runtime-computed view, never persisted.
/// Strings are `&'static str` so the type is allocation-free for the
/// default profiles defined in this crate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CapabilityProfile {
    /// Allowed capability discriminants (stable wire strings).
    pub allowed: BTreeSet<&'static str>,
}

impl CapabilityProfile {
    /// Construct from a static slice of discriminants.
    #[must_use]
    pub fn from_static(items: &[&'static str]) -> Self {
        Self {
            allowed: items.iter().copied().collect(),
        }
    }

    /// True iff the profile contains the given discriminant.
    #[must_use]
    pub fn contains(&self, discriminant: &str) -> bool {
        self.allowed.contains(discriminant)
    }

    /// Set intersection (`self ∩ other`).
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            allowed: self.allowed.intersection(&other.allowed).copied().collect(),
        }
    }

    /// The complete capability discriminant set used by the
    /// `Agent`-mode default. This is the universe of cap names this
    /// crate knows about. Permission narrowing in the permissions
    /// crate uses set-intersection against this list.
    #[must_use]
    pub fn agent_default() -> Self {
        Self::from_static(&[
            "spawnAgent",
            "controlAgent",
            "readAgent",
            "listAgents",
            "manageOrgMembers",
            "manageBilling",
            "invokeProcess",
            "postToFeed",
            "generateMedia",
            "readAllProjects",
            "writeAllProjects",
            "readProject",
            "writeProject",
        ])
    }

    /// Plan: read + markdown writes + read-only subprocess. No spawn,
    /// no media, no billing/membership changes, no broad write.
    #[must_use]
    pub fn plan_default() -> Self {
        Self::from_static(&[
            "readAgent",
            "listAgents",
            "invokeProcess",
            "readAllProjects",
            "readProject",
        ])
    }

    /// Ask: read-only.
    #[must_use]
    pub fn ask_default() -> Self {
        Self::from_static(&["readAgent", "listAgents", "readAllProjects", "readProject"])
    }

    /// Debug: read + sandboxed probes (no subprocess, no write).
    #[must_use]
    pub fn debug_default() -> Self {
        Self::from_static(&["readAgent", "listAgents", "readAllProjects", "readProject"])
    }
}
