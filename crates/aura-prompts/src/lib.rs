//! # aura-prompts
//!
//! Render-only prompt construction layer for the Aura agent harness.
//!
//! This crate owns every model-facing string the harness emits — system
//! prompts, bootstrap user-message context, fix prompts, steering
//! envelopes, compaction-summary auxiliary prompts, and the small set
//! of `&'static str` "model messages" the agent loop and task executor
//! splice into tool-result bodies. After Phase 2 of the core-loop
//! architecture refactor, "where does this user-visible model-facing
//! string come from?" has exactly one answer: somewhere under
//! `crates/aura-prompts/src/`.
//!
//! ## Boundary contract
//!
//! The crate is deliberately **render-only**:
//!
//! - No filesystem IO. The enrichment module in this crate exposes
//!   plain regex extraction + a markdown renderer; the matching IO
//!   half (`WorkspaceReader` trait, real `FsWorkspace`, async
//!   `resolve_hints` orchestration) lives in
//!   `aura-agent/src/prompt_resolve/`.
//! - No build-error analysis. The fix prompt accepts a prepared
//!   [`fix::BuildFixPromptData`] descriptor; classification, error-ref
//!   parsing, and `file_ops::resolve_error_context` stay in
//!   `aura-agent/src/build/` and `aura-agent/src/file_ops/`.
//! - No reasoner-message mutation. Steering rendering produces a
//!   `String` envelope; appending it to `Vec<aura_reasoner::Message>`
//!   is the agent's job (`aura-agent/src/agent_loop/steering/inject.rs`).
//!
//! Cargo.toml forbids `aura-agent`, `aura-automaton`, `aura-runtime`,
//! and `aura-reasoner` so the boundary can never regress at compile
//! time. The workspace-level `tests/prompts_boundary.rs` test does a
//! belt-and-suspenders source-level scan for forbidden imports.
//!
//! ## Surface
//!
//! - [`descriptors`] — borrowed view structs threaded into prompt
//!   builders ([`ProjectInfo`], [`TaskInfo`], [`SpecInfo`],
//!   [`SessionInfo`], [`AgentInfo`], [`AgentIdentity`], [`FileChangeEntry`]).
//! - [`system`] — agentic + chat system-prompt builders, with named
//!   presets ([`system::SystemPromptBuilder::preset_dev_loop`] and
//!   [`system::SystemPromptBuilder::preset_chat`]) and per-section
//!   renderers under [`system::sections`].
//! - [`bootstrap`] — initial user-message construction for an agentic
//!   task run (formerly `prompts/context.rs`).
//! - [`fix`] — formatting-only build-fix prompt builder consuming a
//!   prepared [`fix::BuildFixPromptData`].
//! - [`auxiliary::compaction`] — system prompt + user-prompt template
//!   for the compaction-summary auxiliary LLM call.
//! - [`enrichment`] — pure regex extraction + markdown rendering for
//!   the iteration-0 enrichment block; the IO half lives in
//!   `aura-agent/src/prompt_resolve/`.
//! - [`steering`] — [`steering::SteeringKind`] enum,
//!   [`steering::SteeringRenderer`], and per-kind body wording. The
//!   stateful evaluators (`repeated_read`, `implement_now`,
//!   `early_oracle`) live in `aura-agent/src/agent_loop/steering/`.
//! - [`model_messages`] — `&'static str` constants and small
//!   formatters for every other model-facing string (chunk-guard,
//!   implement-now hard block, max-tokens nudge, task_done
//!   rejections, auto-build feedback, post-task_done test warning).

#![forbid(unsafe_code)]
#![allow(clippy::module_name_repetitions)]
// Prompt-building code uses push_str(&format!()) and format! liberally
// for clarity; tightening these would harm readability.
#![allow(clippy::format_push_string)]
// Internal sub-modules tag many free helpers; documenting every
// `# Errors` clause adds noise without reader value here.
#![allow(clippy::missing_errors_doc)]

pub mod auxiliary;
pub mod bootstrap;
pub mod descriptors;
pub mod enrichment;
pub mod fix;
pub mod model_messages;
pub mod steering;
pub mod system;

pub use descriptors::{
    AgentIdentity, AgentInfo, FileChangeEntry, ProjectInfo, SessionInfo, SpecInfo, TaskInfo,
};
pub use steering::{SteeringKind, SteeringRenderer};
pub use system::{
    agentic_execution_system_prompt, build_chat_system_prompt, default_system_prompt,
    probe_agents_md, AgentsMdProbe, SystemPromptBuilder, CHAT_SYSTEM_PROMPT_BASE,
};
