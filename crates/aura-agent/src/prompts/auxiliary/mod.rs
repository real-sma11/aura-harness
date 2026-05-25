//! Auxiliary LLM-call prompts (compaction summary, etc.).
//!
//! "Auxiliary" means a call to the model that is not the main agent
//! turn — today only the compaction-summary call lives here. Each
//! submodule owns one model-facing string the harness sends so PR D's
//! invariant "every model-facing string lives under
//! `crates/aura-agent/src/prompts/`" holds for non-system-prompt
//! requests too.
//!
//! ## Naming note
//!
//! The original PR D plan used `prompts/aux/`. Windows reserves `AUX`
//! (along with `CON`, `PRN`, `NUL`, `COM1..9`, `LPT1..9`) as a device
//! name regardless of extension and refuses to create files or
//! directories that match, so a literal `aux/` directory never
//! materialises on a Windows checkout. Using `auxiliary` here keeps
//! the module portable across the whole engineering fleet.
//!
//! ## Sub-modules
//!
//! - [`compaction`]: system prompt + user-prompt builder for the
//!   summarisation LLM call invoked by
//!   [`crate::agent_loop::AgentLoop::build_summary_request`].
//!
//! The original PR D plan also called for `auxiliary/decomposition.rs`
//! (housing `SPLITTER_SYSTEM_PROMPT` from
//! `aura-automaton/src/builtins/dev_loop/decomposition.rs`) and
//! `auxiliary/fix_loop.rs` (housing `build_fix_system_prompt`). The
//! Step 1 audit confirmed both were removed before PR D — the
//! `NeedsDecomposition` post-hoc validation gate was dropped by
//! `91e6f7d` and `build_fix_system_prompt` was deleted in PR A — so
//! the corresponding submodules are intentionally absent.

pub mod compaction;
