//! Plugin hook host — convenience surface for runtime call sites.
//!
//! [`PluginHookHost`] bundles the [`HookEngine`] with the
//! `AURA_HOME` path + session id so a runtime call site can fire a
//! hook for any lifecycle event with a one-line invocation. The
//! host is intentionally `Clone` (cheap — just three `Arc` /
//! `PathBuf` fields) so the agent loop can stash a copy in its
//! per-turn state.
//!
//! The `fire_*` helpers all start with the empty-engine
//! short-circuit gate so the empty-install backward-compat
//! invariant is preserved without any caller-side conditionals.

use std::path::PathBuf;
use std::sync::Arc;

use crate::ctx::{
    CtxMeta, PermissionRequestHookCtx, PostCompactHookCtx, PostToolUseHookCtx, PreCompactHookCtx,
    PreToolUseHookCtx, StopHookCtx, UserPromptSubmitHookCtx,
};
use crate::engine::HookEngine;
use crate::event::HookEvent;
use crate::outcome::{AggregateOutcome, HookOutcome};
use crate::redacted::Redacted;

/// Bundle of fields needed to fire any lifecycle hook from a
/// runtime call site. Cheap to clone and pass through trait
/// objects.
#[derive(Clone, Debug)]
pub struct PluginHookHost {
    /// Shared hook engine.
    pub engine: Arc<HookEngine>,
    /// Resolved `AURA_HOME` path (used for env-var injection).
    pub aura_home: PathBuf,
    /// Session id firing the events.
    pub session_id: String,
    /// Agent id firing the events. Each per-event helper accepts an
    /// override so subagent flows can fire under the child id.
    pub agent_id: String,
    /// Optional parent agent id (empty for root agents).
    pub parent_agent_id: Option<String>,
}

impl PluginHookHost {
    /// `true` when no hooks are registered for `event`. The
    /// runtime call sites use this to avoid allocating per-event
    /// ctx structs when nothing will fire.
    #[must_use]
    pub fn is_empty(&self, event: HookEvent) -> bool {
        self.engine.is_empty(event)
    }

    /// Fire [`HookEvent::UserPromptSubmit`].
    ///
    /// Returns the aggregated outcome:
    /// - `Continue` (default): the prompt enters the model context
    ///   unchanged.
    /// - `Replace { new_value }`: the prompt is replaced with
    ///   `new_value` before entering the model context.
    /// - `Block { reason }`: the prompt is dropped; the caller
    ///   should write a `RecordKind::PromptBlockedByHook` audit
    ///   record and skip dispatch.
    pub fn fire_user_prompt_submit(
        &self,
        prompt_text: &str,
        ts: impl Into<String>,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::UserPromptSubmit) {
            return AggregateOutcome::empty();
        }
        let ctx = UserPromptSubmitHookCtx {
            meta: self.meta(),
            prompt_text: Redacted::new(prompt_text.to_string()),
            ts: ts.into(),
        };
        self.engine.fire_event(&ctx, &self.aura_home)
    }

    /// Fire [`HookEvent::PreToolUse`].
    ///
    /// Returns the aggregated outcome. A `Block` decision short-
    /// circuits dispatch with the kernel converting it to
    /// `PolicyVerdict::DeniedByHook`.
    pub fn fire_pre_tool_use(
        &self,
        tool_name: &str,
        tool_args: &str,
        tool_use_id: &str,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::PreToolUse) {
            return AggregateOutcome::empty();
        }
        let ctx = PreToolUseHookCtx {
            meta: self.meta(),
            tool_name: tool_name.to_string(),
            tool_args: Redacted::new(tool_args.to_string()),
            tool_use_id: tool_use_id.to_string(),
        };
        self.engine.fire_event(&ctx, &self.aura_home)
    }

    /// Fire [`HookEvent::PostToolUse`]. Observer-only — the
    /// aggregate outcome is downgraded to `Continue` /
    /// `TimedOut` via [`AggregateOutcome::observe`].
    pub fn fire_post_tool_use(
        &self,
        tool_name: &str,
        tool_use_id: &str,
        effect_status: &str,
        effect_payload_summary: &str,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::PostToolUse) {
            return AggregateOutcome::empty();
        }
        let ctx = PostToolUseHookCtx {
            meta: self.meta(),
            tool_name: tool_name.to_string(),
            tool_use_id: tool_use_id.to_string(),
            effect_status: effect_status.to_string(),
            effect_payload_summary: effect_payload_summary.to_string(),
        };
        self.engine.fire_event(&ctx, &self.aura_home).observe()
    }

    /// Fire [`HookEvent::Stop`]. Observer-only.
    pub fn fire_stop(
        &self,
        total_iterations: u32,
        total_input_tokens: u64,
        total_output_tokens: u64,
        duration_ms: u64,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::Stop) {
            return AggregateOutcome::empty();
        }
        let ctx = StopHookCtx {
            meta: self.meta(),
            total_iterations,
            total_input_tokens,
            total_output_tokens,
            duration_ms,
        };
        self.engine.fire_event(&ctx, &self.aura_home).observe()
    }

    /// Fire [`HookEvent::PreCompact`]. A `Block` decision skips
    /// compaction this turn.
    pub fn fire_pre_compact(
        &self,
        pre_compact_token_count: u64,
        planned_compaction_strategy: &str,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::PreCompact) {
            return AggregateOutcome::empty();
        }
        let ctx = PreCompactHookCtx {
            meta: self.meta(),
            pre_compact_token_count,
            planned_compaction_strategy: planned_compaction_strategy.to_string(),
        };
        self.engine.fire_event(&ctx, &self.aura_home)
    }

    /// Fire [`HookEvent::PostCompact`]. Observer-only.
    pub fn fire_post_compact(
        &self,
        pre_compact_token_count: u64,
        post_compact_token_count: u64,
        summary: &str,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::PostCompact) {
            return AggregateOutcome::empty();
        }
        let ctx = PostCompactHookCtx {
            meta: self.meta(),
            pre_compact_token_count,
            post_compact_token_count,
            summary: summary.to_string(),
        };
        self.engine.fire_event(&ctx, &self.aura_home).observe()
    }

    /// Fire [`HookEvent::PermissionRequest`]. A `Approve` / `Deny`
    /// decision short-circuits the interactive prompt.
    pub fn fire_permission_request(
        &self,
        tool_name: &str,
        tool_args: &str,
        risk_level: &str,
        prompt_text: &str,
    ) -> AggregateOutcome {
        if self.is_empty(HookEvent::PermissionRequest) {
            return AggregateOutcome::empty();
        }
        let ctx = PermissionRequestHookCtx {
            meta: self.meta(),
            tool_name: tool_name.to_string(),
            tool_args: Redacted::new(tool_args.to_string()),
            risk_level: risk_level.to_string(),
            prompt_text: Redacted::new(prompt_text.to_string()),
        };
        self.engine.fire_event(&ctx, &self.aura_home)
    }

    /// Replace the carried `agent_id` (used by subagent flows that
    /// fire an event under the child agent's identity).
    #[must_use]
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = agent_id.into();
        self
    }

    fn meta(&self) -> CtxMeta {
        CtxMeta {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            parent_agent_id: self.parent_agent_id.clone(),
        }
    }
}

/// Convenience: the host's hooks all return `AggregateOutcome`.
/// This helper inspects an outcome's decision and returns true
/// when the call site should treat the firing as a `Block`.
#[must_use]
pub const fn is_blocked(outcome: &AggregateOutcome) -> bool {
    matches!(outcome.decision, HookOutcome::Block { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn host() -> PluginHookHost {
        PluginHookHost {
            engine: Arc::new(HookEngine::new()),
            aura_home: PathBuf::from(if cfg!(windows) {
                r"C:\users\u\.aura"
            } else {
                "/home/u/.aura"
            }),
            session_id: "sess".into(),
            agent_id: "agent".into(),
            parent_agent_id: None,
        }
    }

    #[test]
    fn empty_engine_user_prompt_short_circuits() {
        let h = host();
        let outcome = h.fire_user_prompt_submit("hi", "now");
        assert_eq!(outcome.ran, 0);
        assert_eq!(outcome.decision, HookOutcome::Continue);
    }

    #[test]
    fn empty_engine_pre_tool_short_circuits() {
        let h = host();
        let outcome = h.fire_pre_tool_use("shell", "ls", "call-1");
        assert_eq!(outcome.ran, 0);
    }

    #[test]
    fn empty_engine_permission_request_short_circuits() {
        let h = host();
        let outcome = h.fire_permission_request("shell", "rm", "high", "approve?");
        assert_eq!(outcome.ran, 0);
    }
}
