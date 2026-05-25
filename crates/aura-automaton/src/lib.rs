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
mod runtime;
mod schedule;
mod state;
mod types;

pub mod builtins;

pub use context::TickContext;
pub use error::AutomatonError;
pub use events::AutomatonEvent;
pub use handle::AutomatonHandle;
pub use runtime::{Automaton, AutomatonRuntime, TickOutcome};
pub use schedule::Schedule;
pub use state::AutomatonState;
pub use types::{AutomatonId, AutomatonInfo, AutomatonStatus};

pub use builtins::{ChatAutomaton, DevLoopAutomaton, SpecGenAutomaton, TaskRunAutomaton};
