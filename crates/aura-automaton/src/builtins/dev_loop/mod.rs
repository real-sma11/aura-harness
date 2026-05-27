//! Dev-loop automaton — runs project tasks in order.
//!
//! Intentionally minimal: fetch all tasks on first tick, drop the ones
//! already marked `done`, sort the rest by `order`, then execute one
//! per tick through [`aura_agent::agent_runner::AgentRunner`]. Status
//! transitions are best-effort writes to the domain API. No retries,
//! no dependency graph, no DoD aggregates, no commit gates, no
//! preflight — those layers belong in higher-level orchestration.
//!
//! `mod.rs` owns the [`DevLoopAutomaton`] façade and [`DevLoopConfig`].
//! The Automaton trait impl and per-task execution live in [`tick`].
//! [`forward_event`] translates `aura_agent::AgentLoopEvent` into
//! `AutomatonEvent` for the WS stream and is also re-used by
//! `task_run.rs` and `chat.rs`.

use std::sync::Arc;

use aura_agent::agent_runner::{AgentRunner, AgentRunnerConfig};
use aura_reasoner::ModelProvider;
use aura_tools::catalog::ToolCatalog;
use aura_tools::domain_tools::DomainApi;

use crate::error::AutomatonError;

mod forward_event;
mod tick;

#[cfg(test)]
mod tests;

pub use forward_event::{forward_agent_event, spawn_agent_event_forwarder, ForwardOutcome};

// Per-automaton state keys. Only two cross-tick values survive the
// simplification: the queue of remaining task IDs and an initialized
// flag so the first tick can populate it. Counters are kept so the
// `LoopFinished` terminal event still reports completed/failed totals
// the UI surfaces.
const STATE_INITIALIZED: &str = "initialized";
const STATE_TASK_QUEUE: &str = "task_queue";
const STATE_COMPLETED_COUNT: &str = "completed_count";
const STATE_FAILED_COUNT: &str = "failed_count";
const STATE_LOOP_FINISHED: &str = "loop_finished";

/// Owned mirror of the agent identity / skills / operator-authored
/// system prompt wire bundle, threaded from the
/// `AutomatonStartRequest` JSON config blob into
/// [`aura_agent::agent_runner::AgenticTaskParams::agent`].
///
/// Owns its strings so the `&str`-borrowing
/// [`aura_prompts::AgentInfo`] view handed to the runner can
/// be built on demand from a stable in-memory location. When aura-os
/// leaves the wire fields absent / blank the envelope reports
/// [`Self::is_empty`] and [`Self::as_agent_info`] returns `None`,
/// leaving the assembled system prompt byte-identical to the
/// empty-identity baseline.
#[derive(Debug, Default, Clone)]
pub(crate) struct AgentIdentityEnvelope {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) personality: String,
    pub(crate) skills: Vec<String>,
    pub(crate) system_prompt: Option<String>,
}

impl AgentIdentityEnvelope {
    pub(crate) fn from_json(config: &serde_json::Value) -> Self {
        let identity = config.get("agent_identity");
        let name = identity
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let role = identity
            .and_then(|v| v.get("role"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let personality = identity
            .and_then(|v| v.get("personality"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let skills = config
            .get("agent_skills")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let system_prompt = config
            .get("agent_system_prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
        Self {
            name,
            role,
            personality,
            skills,
            system_prompt,
        }
    }

    /// True when no field carries content. In that state
    /// [`Self::as_agent_info`] returns `None` and the rendered prompt
    /// matches the empty-identity baseline.
    pub(crate) fn is_empty(&self) -> bool {
        self.name.trim().is_empty()
            && self.role.trim().is_empty()
            && self.personality.trim().is_empty()
            && self.skills.iter().all(|s| s.trim().is_empty())
            && self
                .system_prompt
                .as_deref()
                .map_or(true, |s| s.trim().is_empty())
    }

    /// Borrow this envelope as an [`aura_prompts::AgentInfo`].
    /// Returns `None` when [`Self::is_empty`].
    pub(crate) fn as_agent_info(&self) -> Option<aura_prompts::AgentInfo<'_>> {
        if self.is_empty() {
            return None;
        }
        let identity_present = !(self.name.trim().is_empty()
            && self.role.trim().is_empty()
            && self.personality.trim().is_empty());
        let identity = identity_present.then_some(aura_prompts::AgentIdentity {
            name: self.name.as_str(),
            role: self.role.as_str(),
            personality: self.personality.as_str(),
        });
        let system_prompt = self
            .system_prompt
            .as_deref()
            .filter(|s| !s.trim().is_empty());
        Some(aura_prompts::AgentInfo {
            identity,
            skills: self.skills.as_slice(),
            system_prompt,
        })
    }
}

pub(crate) struct DevLoopConfig {
    pub(crate) project_id: String,
    #[allow(dead_code)]
    agent_instance_id: String,
    #[allow(dead_code)]
    model: String,
    /// Identity envelope reinstated on top of the Phase-1 simple loop.
    /// Parsed once from the `AutomatonStartRequest` JSON; the borrowed
    /// `AgentInfo<'_>` view we hand to `AgenticTaskParams::agent` is
    /// derived from these owned buffers via
    /// [`AgentIdentityEnvelope::as_agent_info`]. Stays empty
    /// (`is_empty == true`) until the aura-os populator lands; at
    /// that point identity flows into the rendered system prompt
    /// automatically.
    pub(crate) agent_identity: AgentIdentityEnvelope,
}

impl DevLoopConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, AutomatonError> {
        let project_id = config
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing project_id".into()))?
            .to_string();
        let agent_instance_id = config
            .get("agent_instance_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        // Hard-fail when the operator config didn't pin a model. This
        // used to silently fall back to a build-time `DEFAULT_MODEL`
        // constant — exactly the regression where the
        // `claude-opus-4-7` selection from the chat surface got
        // routed at `claude-opus-4-6` because the dev-loop
        // construction stack reached for the constant. Surface a
        // typed `InvalidConfig` so the operator sees the
        // configuration gap instead of a quiet model swap.
        let model = config
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AutomatonError::InvalidConfig(
                    "missing model — dev-loop requires an explicit model identifier in the start request".into(),
                )
            })?
            .to_string();
        let agent_identity = AgentIdentityEnvelope::from_json(config);
        Ok(Self {
            project_id,
            agent_instance_id,
            model,
            agent_identity,
        })
    }
}

pub struct DevLoopAutomaton {
    pub(crate) domain: Arc<dyn DomainApi>,
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) runner: AgentRunner,
    pub(crate) catalog: Arc<ToolCatalog>,
    pub(crate) tool_executor: Option<Arc<dyn aura_agent::types::AgentToolExecutor>>,
}

impl DevLoopAutomaton {
    /// Construct a dev-loop automaton bound to a kernel-mediated model
    /// provider.
    ///
    /// The `RecordingModelProvider` bound (sealed in `aura-agent`,
    /// Invariant §1 / §3) means external crates can satisfy this only
    /// by passing an `Arc<aura_agent::KernelModelGateway>`, so a raw
    /// HTTP provider can never reach the dev loop without going through
    /// `Kernel::reason_streaming` first.
    pub fn new<P>(
        domain: Arc<dyn DomainApi>,
        provider: Arc<P>,
        config: AgentRunnerConfig,
        catalog: Arc<ToolCatalog>,
    ) -> Self
    where
        P: aura_agent::RecordingModelProvider + Send + Sync + 'static,
    {
        let provider: Arc<dyn ModelProvider> = provider;
        Self {
            domain,
            provider,
            runner: AgentRunner::new(config),
            catalog,
            tool_executor: None,
        }
    }

    #[must_use]
    pub fn with_tool_executor(
        mut self,
        executor: Arc<dyn aura_agent::types::AgentToolExecutor>,
    ) -> Self {
        self.tool_executor = Some(executor);
        self
    }
}
