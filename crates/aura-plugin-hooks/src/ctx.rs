//! Per-event hook context structs.
//!
//! Phase 8 splits the original [`crate::HookFiringContext`] (which
//! served Phase 4c manual fires) into one strongly-typed struct per
//! lifecycle event. The dedicated structs document the data shape
//! handlers see + use the [`crate::Redacted`] wrapper to keep
//! sensitive payloads (tool args, prompt text) out of structured
//! logs.
//!
//! ## Invariants ([rules.md §13])
//!
//! - Sensitive fields (tool args, prompt text) MUST be wrapped in
//!   [`crate::Redacted`]. Plain `String` is reserved for non-secret
//!   metadata.
//! - Every ctx implements [`HookCtx::event`] so the engine can fire
//!   handlers without taking the variant in two places.
//! - `Debug` derivation is mandatory on every type per
//!   `.cursor/rules.md` §rules. Sensitive fields use the
//!   [`Redacted`] wrapper to redact during `Debug`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::event::HookEvent;
use crate::redacted::Redacted;

/// Reference to an enabled plugin from a hook firing perspective.
/// Kept minimal so the SessionStart payload doesn't drag the full
/// plugin manifest into hook handler subprocesses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRef {
    /// Plugin id (matches the manifest `id` field).
    pub id: String,
    /// Plugin version as a semver string.
    pub version: String,
}

/// Per-plugin failure surfaced during session-start materialisation.
/// Reported to handlers via [`SessionStartHookCtx::plugin_load_failures`]
/// so an enterprise audit hook can react to a partial-load session.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginLoadFailure {
    /// Plugin id that failed to load.
    pub plugin_id: String,
    /// Human-readable reason for the failure.
    pub reason: String,
}

/// Common metadata carried by every hook context. Bundled to avoid
/// duplicating the four fields across ten structs.
#[derive(Clone, Debug)]
pub struct CtxMeta {
    /// Session id firing the hook.
    pub session_id: String,
    /// Agent id firing the hook.
    pub agent_id: String,
    /// Optional parent agent id. Empty string for root agents (the
    /// env-var injection path treats `None` and `Some("")` identically
    /// — the hook subprocess sees `AURA_PARENT_AGENT_ID=""`).
    pub parent_agent_id: Option<String>,
}

/// Trait implemented by every per-event ctx so the firing loop can
/// reach the event tag without `match`-ing on a hand-rolled wrapper
/// enum. Phase 8 keeps this trait `pub` to allow consumers to write
/// generic helpers (e.g. a test harness).
pub trait HookCtx {
    /// Lifecycle event this ctx is for.
    fn event(&self) -> HookEvent;
    /// Common metadata for env-var injection.
    fn meta(&self) -> &CtxMeta;
    /// Extra hook-process env vars contributed by the firing site.
    /// Empty for most events. Always merged AFTER the canonical
    /// Aura / Codex / Claude vars so a firing site can override
    /// canonical names if it really wants to (rarely needed).
    fn extra_env(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
}

/// Ctx for [`HookEvent::SessionStart`].
#[derive(Clone, Debug)]
pub struct SessionStartHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// `AgentMode` snake_case spelling, e.g. `"agent"`, `"plan"`.
    pub mode: String,
    /// Resolved primary model id.
    pub model_id: String,
    /// List of plugins materialised at session start.
    pub enabled_plugins: Vec<PluginRef>,
    /// Per-plugin failures (best-effort load).
    pub plugin_load_failures: Vec<PluginLoadFailure>,
}

impl HookCtx for SessionStartHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::SessionStart
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
}

/// Ctx for [`HookEvent::UserPromptSubmit`]. Handlers MAY return
/// [`crate::HookOutcome::Replace`] to mutate the prompt or
/// [`crate::HookOutcome::Block`] to drop the prompt.
#[derive(Clone, Debug)]
pub struct UserPromptSubmitHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Prompt text. Wrapped in [`Redacted`] so accidental
    /// `tracing::debug!(?ctx)` does not echo the user's prompt.
    pub prompt_text: Redacted<String>,
    /// Submission timestamp (RFC3339).
    pub ts: String,
}

impl HookCtx for UserPromptSubmitHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::UserPromptSubmit
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
}

/// Ctx for [`HookEvent::PreToolUse`]. Handlers MAY return
/// [`crate::HookOutcome::Block`] to reject the tool call.
#[derive(Clone, Debug)]
pub struct PreToolUseHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Tool name about to be dispatched.
    pub tool_name: String,
    /// Tool args; redacted to keep secrets / large payloads out of
    /// structured logs.
    pub tool_args: Redacted<String>,
    /// Stable tool-call id (matches the model's emitted id).
    pub tool_use_id: String,
}

impl HookCtx for PreToolUseHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::PreToolUse
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AURA_TOOL_NAME".into(), self.tool_name.clone());
        env.insert("AURA_TOOL_USE_ID".into(), self.tool_use_id.clone());
        env
    }
}

/// Ctx for [`HookEvent::PostToolUse`]. Observer-only.
#[derive(Clone, Debug)]
pub struct PostToolUseHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Tool name that just dispatched.
    pub tool_name: String,
    /// Tool-call id matching the prior `PreToolUse` firing.
    pub tool_use_id: String,
    /// Effect status — `"ok"`, `"error"`, `"denied"`, etc.
    pub effect_status: String,
    /// Short summary of the effect payload (NOT the full payload —
    /// this is observer-grade metadata only).
    pub effect_payload_summary: String,
}

impl HookCtx for PostToolUseHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::PostToolUse
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AURA_TOOL_NAME".into(), self.tool_name.clone());
        env.insert("AURA_TOOL_USE_ID".into(), self.tool_use_id.clone());
        env.insert("AURA_EFFECT_STATUS".into(), self.effect_status.clone());
        env
    }
}

/// Ctx for [`HookEvent::SubagentStart`]. Observer-only.
#[derive(Clone, Debug)]
pub struct SubagentStartHookCtx {
    /// Common identifiers (parent_agent_id holds the parent).
    pub meta: CtxMeta,
    /// Child agent id newly assigned by the spawner.
    pub child_id: String,
    /// AgentMode snake_case spelling for the child.
    pub mode: String,
    /// KernelMode snake_case spelling for the child.
    pub kernel_mode: String,
    /// JSON serialisation of the override manifest. Compact so the
    /// spawned process can `JSON.parse(env.AURA_OVERRIDE_MANIFEST)`.
    pub override_manifest: String,
}

impl HookCtx for SubagentStartHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::SubagentStart
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AURA_CHILD_AGENT_ID".into(), self.child_id.clone());
        env.insert("AURA_CHILD_MODE".into(), self.mode.clone());
        env.insert("AURA_CHILD_KERNEL_MODE".into(), self.kernel_mode.clone());
        env.insert(
            "AURA_OVERRIDE_MANIFEST".into(),
            self.override_manifest.clone(),
        );
        env
    }
}

/// Ctx for [`HookEvent::SubagentStop`]. Observer-only.
#[derive(Clone, Debug)]
pub struct SubagentStopHookCtx {
    /// Common identifiers (parent_agent_id holds the parent).
    pub meta: CtxMeta,
    /// Child agent id that completed.
    pub child_id: String,
    /// Short outcome summary (`"completed"`, `"failed: …"`, etc.).
    pub result_summary: String,
    /// Wall-clock duration (ms).
    pub duration_ms: u64,
}

impl HookCtx for SubagentStopHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::SubagentStop
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AURA_CHILD_AGENT_ID".into(), self.child_id.clone());
        env.insert(
            "AURA_CHILD_RESULT_SUMMARY".into(),
            self.result_summary.clone(),
        );
        env.insert(
            "AURA_CHILD_DURATION_MS".into(),
            self.duration_ms.to_string(),
        );
        env
    }
}

/// Ctx for [`HookEvent::Stop`]. Observer-only.
#[derive(Clone, Debug)]
pub struct StopHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Total turn iterations consumed.
    pub total_iterations: u32,
    /// Total input tokens consumed.
    pub total_input_tokens: u64,
    /// Total output tokens consumed.
    pub total_output_tokens: u64,
    /// Wall-clock duration (ms).
    pub duration_ms: u64,
}

impl HookCtx for StopHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::Stop
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "AURA_TOTAL_ITERATIONS".into(),
            self.total_iterations.to_string(),
        );
        env.insert(
            "AURA_TOTAL_INPUT_TOKENS".into(),
            self.total_input_tokens.to_string(),
        );
        env.insert(
            "AURA_TOTAL_OUTPUT_TOKENS".into(),
            self.total_output_tokens.to_string(),
        );
        env.insert("AURA_DURATION_MS".into(), self.duration_ms.to_string());
        env
    }
}

/// Ctx for [`HookEvent::PreCompact`]. Handlers MAY return
/// [`crate::HookOutcome::Block`] to skip compaction this turn.
#[derive(Clone, Debug)]
pub struct PreCompactHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Token count immediately before compaction.
    pub pre_compact_token_count: u64,
    /// Compaction strategy (`"summary"`, `"truncate-tail"`, etc.).
    pub planned_compaction_strategy: String,
}

impl HookCtx for PreCompactHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::PreCompact
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "AURA_PRE_COMPACT_TOKEN_COUNT".into(),
            self.pre_compact_token_count.to_string(),
        );
        env.insert(
            "AURA_COMPACTION_STRATEGY".into(),
            self.planned_compaction_strategy.clone(),
        );
        env
    }
}

/// Ctx for [`HookEvent::PostCompact`]. Observer-only.
#[derive(Clone, Debug)]
pub struct PostCompactHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Token count immediately before compaction.
    pub pre_compact_token_count: u64,
    /// Token count immediately after compaction.
    pub post_compact_token_count: u64,
    /// Short summary of the compaction outcome.
    pub summary: String,
}

impl HookCtx for PostCompactHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::PostCompact
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "AURA_PRE_COMPACT_TOKEN_COUNT".into(),
            self.pre_compact_token_count.to_string(),
        );
        env.insert(
            "AURA_POST_COMPACT_TOKEN_COUNT".into(),
            self.post_compact_token_count.to_string(),
        );
        env
    }
}

/// Ctx for [`HookEvent::PermissionRequest`]. Handlers MAY return
/// [`crate::HookOutcome::Approve`] / [`crate::HookOutcome::Deny`] to
/// short-circuit an interactive prompt.
#[derive(Clone, Debug)]
pub struct PermissionRequestHookCtx {
    /// Common identifiers.
    pub meta: CtxMeta,
    /// Tool name being requested.
    pub tool_name: String,
    /// Tool args; redacted.
    pub tool_args: Redacted<String>,
    /// Risk classification (`"low"`, `"medium"`, `"high"`).
    pub risk_level: String,
    /// Human-readable prompt text the operator would see.
    pub prompt_text: Redacted<String>,
}

impl HookCtx for PermissionRequestHookCtx {
    fn event(&self) -> HookEvent {
        HookEvent::PermissionRequest
    }
    fn meta(&self) -> &CtxMeta {
        &self.meta
    }
    fn extra_env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("AURA_TOOL_NAME".into(), self.tool_name.clone());
        env.insert("AURA_RISK_LEVEL".into(), self.risk_level.clone());
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> CtxMeta {
        CtxMeta {
            session_id: "sess-1".into(),
            agent_id: "agent-1".into(),
            parent_agent_id: None,
        }
    }

    #[test]
    fn user_prompt_submit_redacts_in_debug() {
        let ctx = UserPromptSubmitHookCtx {
            meta: meta(),
            prompt_text: Redacted::new("tell me a secret".into()),
            ts: "2024-01-01T00:00:00Z".into(),
        };
        let dbg = format!("{ctx:?}");
        assert!(!dbg.contains("tell me a secret"), "got {dbg}");
        assert!(dbg.contains("<REDACTED>"));
    }

    #[test]
    fn pre_tool_use_emits_extra_env() {
        let ctx = PreToolUseHookCtx {
            meta: meta(),
            tool_name: "shell".into(),
            tool_args: Redacted::new("rm -rf /".into()),
            tool_use_id: "call-42".into(),
        };
        let env = ctx.extra_env();
        assert_eq!(env.get("AURA_TOOL_NAME").map(String::as_str), Some("shell"));
        assert_eq!(
            env.get("AURA_TOOL_USE_ID").map(String::as_str),
            Some("call-42")
        );
    }

    #[test]
    fn permission_request_redacts_both_secrets_in_debug() {
        let ctx = PermissionRequestHookCtx {
            meta: meta(),
            tool_name: "shell".into(),
            tool_args: Redacted::new("super-secret".into()),
            risk_level: "high".into(),
            prompt_text: Redacted::new("approve dangerous?".into()),
        };
        let dbg = format!("{ctx:?}");
        assert!(!dbg.contains("super-secret"));
        assert!(!dbg.contains("approve dangerous?"));
    }
}
