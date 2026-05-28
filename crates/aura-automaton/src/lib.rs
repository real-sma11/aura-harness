//! # aura-automaton
//!
//! Layer: surface
//!
//! Headless automaton host: built-in flows (`DevLoopAutomaton`,
//! `SpecGenAutomaton`, `TaskRunAutomaton`), the `AutomatonRuntime`
//! scheduler, the `TickContext` injection point, and the
//! `AutomatonEvent` event surface. Phase 9 reclassifies this crate
//! at the surface layer; the surface-layer relocation shell
//! `aura-surface-automaton` re-exports it so layer-boundary tests
//! see a single surface-layer entry while consumers migrate.
#![forbid(unsafe_code)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::option_if_let_else)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::needless_pass_by_ref_mut)] // Future-proofing for mutable references
#![allow(clippy::match_wildcard_for_single_variants)] // Wildcard is intentional for forward compatibility

mod context;
mod error;
mod events;
mod handle;
mod metadata;
mod runtime;
mod state;
mod types;

pub mod builtins;

pub use context::TickContext;
// `AutomatonError` is internal to the crate (no external crate names it
// directly — `aura-runtime` only consumes it via Display in `.map_err`).
// Kept as `pub(crate) use` so the in-crate path `crate::AutomatonError`
// keeps working without bumping the public API surface.
pub(crate) use error::AutomatonError;
pub use events::AutomatonEvent;
pub use handle::AutomatonHandle;
pub use runtime::{Automaton, AutomatonRuntime, TickOutcome};
// `Schedule` (in `crate::metadata`), `AutomatonStatus`, `ChatAutomaton`
// are internal to the crate; nothing outside `aura-automaton` references
// them by name. Their underlying `pub` definitions stay reachable
// through `AutomatonInfo` fields / trait return types without being
// part of the crate's named public surface.
pub use state::AutomatonState;
pub use types::{AutomatonId, AutomatonInfo};

pub use builtins::{DevLoopAutomaton, SpecGenAutomaton, TaskRunAutomaton};
