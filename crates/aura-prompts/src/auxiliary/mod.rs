//! Auxiliary LLM-call prompts (compaction summary, etc.).
//!
//! "Auxiliary" means a call to the model that is not the main agent
//! turn — today only the compaction-summary call lives here. Each
//! submodule owns one model-facing string the harness sends so the
//! invariant "every model-facing string lives under
//! `crates/aura-prompts/src/`" holds for non-system-prompt requests
//! too.
//!
//! ## Naming note
//!
//! The original PR D plan used `prompts/aux/`. Windows reserves `AUX`
//! (along with `CON`, `PRN`, `NUL`, `COM1..9`, `LPT1..9`) as a device
//! name regardless of extension and refuses to create files or
//! directories that match, so a literal `aux/` directory never
//! materialises on a Windows checkout. Using `auxiliary` here keeps
//! the module portable across the whole engineering fleet.

pub mod compaction;
