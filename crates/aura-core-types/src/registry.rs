//! Unified `Registry` trait for name-keyed lookup stores.
//!
//! Wave 4 consolidates the duplicated `name -> item` lookups that had
//! sprouted across the workspace:
//!
//! - `aura_skills::SkillRegistry` — in-memory map of loaded skills,
//!   populated from a `SkillLoader`.
//! - `aura_tools::ToolCatalog` — immutable, compile-time–constructed
//!   catalog of tool metadata. Implements the read-only slice of
//!   [`Registry`] (`get` / `iter` / `len`) and reports `Unsupported`
//!   for mutating calls; callers that need profile- or
//!   capability-filtered views must use the inherent methods.
//! - `aura_automaton::AutomatonRuntime` — running automaton instances
//!   keyed by their id string.
//!
//! Implementors keep their inherent methods for compatibility; the trait
//! gives call sites a single abstraction to reach for when they only need
//! `get` / `iter` / `len` / `is_empty` across heterogeneous registries,
//! and it documents the shared semantics. Because concrete registries
//! differ in their internal storage (plain `HashMap`, sharded `DashMap`
//! guarded behind locks), the trait exposes *snapshot / clone*-based
//! access instead of forcing every implementor to hand out a long-lived
//! borrow. Hot paths continue to use the inherent (borrow-returning)
//! methods.

use std::fmt::Debug;
use std::hash::Hash;

/// Errors returned by [`Registry`] mutation methods.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// An entry with the same id is already registered.
    #[error("duplicate id: {0}")]
    Duplicate(String),

    /// This registry does not support the mutation (e.g. it is populated
    /// by a dedicated lifecycle API and rejects ad-hoc inserts).
    #[error("unsupported mutation: {0}")]
    Unsupported(&'static str),
}

/// Abstraction over `Id -> Item` lookup stores.
///
/// `get` and `iter` return owned values (via `Clone`). Implementors back
/// their storage with plain maps (`HashMap`), sharded maps (`DashMap`),
/// or other structures; returning clones is the least-common-denominator
/// that works uniformly. Callers that need to avoid cloning should use
/// the concrete type's inherent methods.
pub trait Registry {
    /// The lookup key.
    type Id: Eq + Hash + Clone + Debug;
    /// The stored item. Must be cloneable because implementors may keep
    /// their items behind a lock guard and cannot hand out borrows.
    type Item: Clone;

    /// Register a new entry. Implementors that do not support ad-hoc
    /// inserts return [`RegistryError::Unsupported`].
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::Duplicate`] if an entry with this id is
    /// already registered, or [`RegistryError::Unsupported`] if this
    /// registry does not accept direct inserts.
    fn register(&mut self, id: Self::Id, item: Self::Item) -> Result<(), RegistryError>;

    /// Look up an entry by id, cloning the stored value.
    fn get(&self, id: &Self::Id) -> Option<Self::Item>;

    /// Snapshot of `(id, item)` pairs. Order is unspecified.
    fn iter(&self) -> Vec<(Self::Id, Self::Item)>;

    /// Remove and return an entry by id, if present.
    ///
    /// Implementors that do not support ad-hoc removal return `None`
    /// rather than erroring.
    fn remove(&mut self, id: &Self::Id) -> Option<Self::Item>;

    /// Number of registered entries.
    fn len(&self) -> usize {
        self.iter().len()
    }

    /// `true` when no entries are registered.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
