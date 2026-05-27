//! Typed configuration parsed from the `AutomatonStartRequest` JSON
//! blob.
//!
//! Before Phase 6 the dev-loop reparsed its config on **every tick**
//! and the `AgentIdentityEnvelope` lived inside `dev_loop/mod.rs` even
//! though `task_run` and `chat` also wanted access to it. This module
//! centralizes both:
//!
//! - [`AgentIdentityEnvelope`] — owned mirror of the agent identity
//!   bundle the operator sends through the wire. Renders as the
//!   transient `aura_prompts::AgentInfo<'_>` view the prompt builders
//!   consume.
//! - [`DevLoopConfig`] — the dev-loop's typed start-request config.
//!   Parsed **once** in `on_install` and stashed on the
//!   `DevLoopAutomaton` struct so per-tick code never reaches back
//!   into the JSON bag.

use std::sync::Arc;

use serde::Deserialize;

use crate::error::AutomatonError;

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

/// Typed dev-loop start-request config.
///
/// Parsed once in [`crate::builtins::dev_loop::DevLoopAutomaton::on_install`]
/// and stashed on the automaton via an `Arc<DevLoopConfig>` so per-tick
/// code reads from a typed struct instead of reaching back into the
/// JSON bag. The previous "parse on every tick" pattern made it easy
/// to drift the field set between sites and recomputed the
/// identity-envelope buffers `tick.len()` times.
#[derive(Debug)]
pub(crate) struct DevLoopConfig {
    pub(crate) project_id: String,
    #[allow(dead_code)]
    pub(crate) agent_instance_id: String,
    pub(crate) model: String,
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

/// Tightly-typed view used by `serde` so the field set is checked at
/// compile time without exposing the inner shape to dev-loop consumers
/// (which only ever read the public `DevLoopConfig` projection above).
#[derive(Deserialize)]
struct DevLoopConfigRaw {
    project_id: Option<String>,
    agent_instance_id: Option<String>,
    model: Option<String>,
}

impl DevLoopConfig {
    /// Parse the `AutomatonStartRequest` JSON blob exactly once.
    ///
    /// Returns [`AutomatonError::InvalidConfig`] when `project_id` is
    /// missing or when `model` is missing / blank. The pre-Phase-1
    /// behaviour silently fell back to a build-time `DEFAULT_MODEL`
    /// constant — exactly the regression where the
    /// `claude-opus-4-7` selection from the chat surface got routed
    /// at `claude-opus-4-6` because the dev-loop construction stack
    /// reached for the constant. Surface a typed `InvalidConfig` so
    /// the operator sees the configuration gap instead of a quiet
    /// model swap.
    pub(crate) fn from_json(config: &serde_json::Value) -> Result<Self, AutomatonError> {
        let raw: DevLoopConfigRaw = serde_json::from_value(config.clone())
            .map_err(|e| AutomatonError::InvalidConfig(format!("invalid dev-loop config: {e}")))?;

        let project_id = raw
            .project_id
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing project_id".into()))?;
        let agent_instance_id = raw
            .agent_instance_id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "default".to_string());
        let model = raw
            .model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AutomatonError::InvalidConfig(
                    "missing model — dev-loop requires an explicit model identifier in the start request".into(),
                )
            })?;
        let agent_identity = AgentIdentityEnvelope::from_json(config);
        Ok(Self {
            project_id,
            agent_instance_id,
            model,
            agent_identity,
        })
    }
}

/// Convenience alias for the parsed, shared dev-loop config stashed
/// on the [`crate::builtins::DevLoopAutomaton`] struct.
pub(crate) type SharedDevLoopConfig = Arc<DevLoopConfig>;
