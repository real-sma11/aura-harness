//! Session state management.
//!
//! This file owns the [`Session`] struct plus everything that maintains
//! its per-connection state: `new`, `apply_init`, the wire→core permission
//! translator, the intent-classifier builder, `AgentLoopConfig` derivation,
//! and the agent loop configuration derived from session state. Split out of
//! `session/mod.rs` in Wave 6 / T3 so `mod.rs` can stay
//! tiny (declarations + `WsContext` + re-exports).

use crate::protocol::{self, SessionInit};
use crate::session::ToolApprovalBroker;
use aura_agent::{prompts::default_system_prompt, AgentLoopConfig};
use aura_core::{
    AgentId, AgentPermissions, AgentScope, AgentToolPermissions, Capability,
    InstalledIntegrationDefinition, InstalledToolDefinition,
};
use aura_protocol::{
    AgentPermissionsWire, CapabilityWire, IntentClassifierSpec, SessionModelOverrides,
};
use aura_reasoner::{Message, ModelProvider, ToolDefinition};
use aura_tools::IntentClassifier;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Session
// ============================================================================

/// Per-connection session state.
///
/// Fields are `pub(crate)` so only the node crate may mutate them; external
/// crates must go through the public accessors / constructors we expose on
/// purpose. (Wave 3 — T2.2.)
pub struct Session {
    /// Unique session identifier.
    pub(crate) session_id: String,
    /// Stable agent ID for the lifetime of this session.
    pub(crate) agent_id: AgentId,
    /// System prompt for the model.
    pub(crate) system_prompt: String,
    /// Model identifier.
    pub(crate) model: String,
    /// Provider identifier for this session.
    pub(crate) provider_name: String,
    /// Optional per-session model overrides resolved from `session_init`.
    pub(crate) provider_overrides: Option<SessionModelOverrides>,
    /// Stable OpenAI-family `prompt_cache_key` resolved from `SessionInit.provider_overrides`.
    pub(crate) prompt_cache_key: Option<String>,
    /// Optional OpenAI-family `prompt_cache_retention` paired with `prompt_cache_key`.
    pub(crate) prompt_cache_retention: Option<String>,
    /// Optional concrete provider override built from `provider_overrides`.
    pub(crate) provider_override: Option<Arc<dyn ModelProvider + Send + Sync>>,
    /// Max tokens per response.
    pub(crate) max_tokens: u32,
    /// Sampling temperature.
    pub(crate) temperature: Option<f32>,
    /// Maximum agentic steps per turn.
    ///
    /// Defaults to `u32::MAX` (effectively unlimited). The runtime maps
    /// `u32::MAX` to `usize::MAX` in [`Session::agent_loop_config`] so
    /// the agent loop's iteration check short-circuits and termination
    /// is driven by `EndTurn` from the model, the credit/token budget,
    /// or cooperative cancellation. Callers wanting bounded turns must
    /// pass an explicit `SessionInit.max_turns`.
    pub(crate) max_turns: u32,
    /// Installed tools registered for this session.
    pub(crate) installed_tools: Vec<InstalledToolDefinition>,
    /// Installed integrations authorized for this session.
    pub(crate) installed_integrations: Vec<InstalledIntegrationDefinition>,
    /// Conversation history (accumulated across turns).
    pub(crate) messages: Vec<Message>,
    /// Cumulative input tokens across all turns.
    pub(crate) cumulative_input_tokens: u64,
    /// Cumulative output tokens across all turns.
    pub(crate) cumulative_output_tokens: u64,
    /// Cumulative cache creation input tokens across all turns.
    pub(crate) cumulative_cache_creation_input_tokens: u64,
    /// Cumulative cache read input tokens across all turns.
    pub(crate) cumulative_cache_read_input_tokens: u64,
    /// Workspace directory for this session (sandboxed fallback).
    pub(crate) workspace: PathBuf,
    /// Base directory that workspace must reside under.
    pub(crate) workspace_base: PathBuf,
    /// Real project directory on the host filesystem.
    /// When set, tool execution uses this path directly.
    pub(crate) project_path: Option<PathBuf>,
    /// Optional base directory that project_path must reside under (remote VM mode).
    pub(super) project_base: Option<PathBuf>,
    /// Whether `session_init` has been received.
    pub(crate) initialized: bool,
    /// Available tool definitions (builtin + external).
    pub(crate) tool_definitions: Vec<ToolDefinition>,
    /// Context window size in tokens (for utilization calculation).
    pub(crate) context_window_tokens: u64,
    /// JWT auth token for proxy routing.
    pub(crate) auth_token: Option<String>,
    /// Project ID for domain tool calls.
    pub(crate) project_id: Option<String>,
    /// Project-agent UUID for X-Aura-Agent-Id billing header.
    pub(crate) aura_agent_id: Option<String>,
    /// Storage session UUID for X-Aura-Session-Id billing header.
    pub(crate) aura_session_id: Option<String>,
    /// Org UUID for X-Aura-Org-Id billing header.
    pub(crate) aura_org_id: Option<String>,
    /// Harness-level agent ID for per-agent skill lookup.
    pub(crate) skill_agent_id: Option<String>,
    /// Optional keyword-driven intent classifier that narrows the visible
    /// tool set per turn. Populated from
    /// [`aura_protocol::SessionInit::intent_classifier`] so a
    /// harness-hosted super-agent can reproduce the aura-os tier-1/tier-2
    /// filtering behavior without the harness binary knowing the manifest.
    pub(crate) intent_classifier: Option<Arc<IntentClassifier>>,
    /// `(tool_name, domain)` pairs paired with [`intent_classifier`]. Empty
    /// when the classifier is not configured.
    ///
    /// [`intent_classifier`]: Self::intent_classifier
    pub(crate) intent_classifier_manifest: Vec<(String, String)>,
    /// Agent permissions for this session, derived directly from the
    /// required `SessionInit.agent_permissions` field. Always applied to
    /// the kernel [`aura_kernel::PolicyConfig`] on kernel construction;
    /// enforcement is unconditional.
    pub(crate) agent_permissions: AgentPermissions,
    /// Originating user id for tool-default resolution and forever approvals.
    pub(crate) user_id: String,
    /// Optional per-agent tool override for this session.
    pub(crate) tool_permissions: Option<AgentToolPermissions>,
    /// Live approval broker attached to this WebSocket connection.
    pub(crate) tool_approval_broker: Option<Arc<ToolApprovalBroker>>,
}

impl Session {
    /// Create a new uninitialized session with defaults.
    pub(super) fn new(default_workspace: PathBuf) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            agent_id: AgentId::generate(),
            system_prompt: String::new(),
            model: aura_agent::DEFAULT_MODEL.to_string(),
            provider_name: String::new(),
            provider_overrides: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            provider_override: None,
            max_tokens: 16384,
            temperature: None,
            // Effectively unlimited. See the field doc on `max_turns`
            // for rationale and termination signals.
            max_turns: u32::MAX,
            installed_tools: Vec::new(),
            installed_integrations: Vec::new(),
            messages: Vec::new(),
            cumulative_input_tokens: 0,
            cumulative_output_tokens: 0,
            cumulative_cache_creation_input_tokens: 0,
            cumulative_cache_read_input_tokens: 0,
            workspace: default_workspace.clone(),
            workspace_base: default_workspace,
            project_path: None,
            project_base: None,
            initialized: false,
            tool_definitions: Vec::new(),
            context_window_tokens: 200_000,
            auth_token: None,
            project_id: None,
            aura_agent_id: None,
            aura_session_id: None,
            aura_org_id: None,
            skill_agent_id: None,
            intent_classifier: None,
            intent_classifier_manifest: Vec::new(),
            agent_permissions: AgentPermissions::full_access(),
            user_id: String::new(),
            tool_permissions: None,
            tool_approval_broker: None,
        }
    }

    /// Apply a `session_init` message to configure this session.
    pub(super) fn apply_init(&mut self, init: SessionInit) -> Result<(), String> {
        if let Some(prompt) = init.system_prompt {
            self.system_prompt = prompt;
        }
        if let Some(model) = init.model {
            self.context_window_tokens = context_window_for_model(&model);
            self.model = model;
        }
        if let Some(max_tokens) = init.max_tokens {
            self.max_tokens = max_tokens;
        }
        if let Some(temperature) = init.temperature {
            self.temperature = Some(temperature);
        }
        if let Some(max_turns) = init.max_turns {
            self.max_turns = max_turns;
        }
        if let Some(tools) = init.installed_tools {
            self.installed_tools = tools
                .into_iter()
                .map(protocol::installed_tool_to_core)
                .collect();
        }
        if let Some(integrations) = init.installed_integrations {
            self.installed_integrations = integrations
                .into_iter()
                .map(protocol::installed_integration_to_core)
                .collect();
        }
        if let Some(workspace) = init.workspace {
            let candidate = PathBuf::from(&workspace);
            if candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("workspace path must not contain '..' components".into());
            }
            let normalized = lexical_normalize(&candidate);
            let normalized_base = lexical_normalize(&self.workspace_base);
            if !normalized.starts_with(&normalized_base) {
                return Err(format!(
                    "workspace path must be under {}",
                    self.workspace_base.display()
                ));
            }
            self.workspace = candidate;
        }
        if let Some(ref pp) = init.project_path {
            let candidate = PathBuf::from(pp);
            if !candidate.is_absolute() {
                return Err("project_path must be an absolute path".into());
            }
            if candidate
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err("project_path must not contain '..' components".into());
            }
            // When project_base is configured (remote VM mode), validate that
            // the project path is under it to prevent sandbox escape.
            if let Some(ref base) = self.project_base {
                let normalized = lexical_normalize(&candidate);
                let normalized_base = lexical_normalize(base);
                if !normalized.starts_with(&normalized_base) {
                    return Err(format!("project_path must be under {}", base.display()));
                }
            }
            self.project_path = Some(candidate);
        }
        if let Some(token) = init.token {
            self.auth_token = Some(token);
        }
        if let Some(agent_id) = init.agent_id {
            self.skill_agent_id = Some(
                init.template_agent_id
                    .as_ref()
                    .filter(|id| !id.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| agent_id.clone()),
            );
            self.agent_id = AgentId::from_hex(&agent_id).unwrap_or_else(|_| {
                let hash = blake3::hash(agent_id.as_bytes());
                AgentId::new(*hash.as_bytes())
            });
        } else if let Some(template_agent_id) = init.template_agent_id {
            if !template_agent_id.trim().is_empty() {
                self.skill_agent_id = Some(template_agent_id);
            }
        }
        if init.user_id.trim().is_empty() {
            return Err("user_id is required".into());
        }
        self.user_id = init.user_id;
        self.tool_permissions = init
            .tool_permissions
            .map(protocol::agent_tool_permissions_from_wire);
        if let Some(pid) = init.project_id {
            self.project_id = Some(pid);
        }
        if let Some(id) = init.aura_agent_id {
            self.aura_agent_id = Some(id);
        }
        if let Some(id) = init.aura_session_id {
            self.aura_session_id = Some(id);
        }
        if let Some(id) = init.aura_org_id {
            self.aura_org_id = Some(id);
        }
        if let Some(provider_overrides) = init.provider_overrides {
            self.prompt_cache_key = provider_overrides.prompt_cache_key.clone();
            self.prompt_cache_retention = provider_overrides.prompt_cache_retention.clone();
            self.provider_overrides = Some(provider_overrides);
        }
        if let Some(spec) = init.intent_classifier {
            let (classifier, manifest) = build_intent_classifier(spec);
            self.intent_classifier = Some(Arc::new(classifier));
            self.intent_classifier_manifest = manifest;
        }

        // Agent permissions are applied once at session init. The canonical
        // default is full access; callers that need restrictions must send a
        // narrower non-default bundle.
        self.agent_permissions = agent_permissions_from_wire(init.agent_permissions);
        if let Some(msgs) = init.conversation_messages {
            for msg in msgs {
                match msg.role.as_str() {
                    "user" => self.messages.push(Message::user(&msg.content)),
                    "assistant" => self.messages.push(Message::assistant(&msg.content)),
                    _ => {}
                }
            }
        }
        self.initialized = true;
        Ok(())
    }

    /// Return a deterministic `AgentId` for memory keying.
    ///
    /// When the session carries an `aura_agent_id` (the aura-os UUID),
    /// derive the `AgentId` from it so memory queries from the UI use the
    /// same key. Falls back to the random session `agent_id`.
    pub(super) fn memory_agent_id(&self) -> AgentId {
        if let Some(ref uuid_str) = self.aura_agent_id {
            if let Ok(uuid) = uuid::Uuid::parse_str(uuid_str) {
                return AgentId::from_uuid(uuid);
            }
        }
        self.agent_id
    }

    /// Build an `AgentLoopConfig` from session state.
    pub(super) fn agent_loop_config(&self) -> AgentLoopConfig {
        let base_prompt = if self.system_prompt.is_empty() {
            default_system_prompt()
        } else {
            self.system_prompt.clone()
        };

        let system_prompt = if let Some(ref pp) = self.project_path {
            format!(
                "{base_prompt}\n\n## Workspace\n\n\
                 Your workspace root is `{}`. All relative file paths are resolved against this directory. \
                 When referring to files, use paths relative to this root.",
                pp.display()
            )
        } else {
            base_prompt
        };

        // Wire-protocol `max_turns` is `u32`; map the `u32::MAX`
        // sentinel to `usize::MAX` so the agent loop's unlimited-mode
        // short-circuit (see `aura_agent::budget::should_stop_for_budget`
        // and `aura_agent::agent_loop::context::check_budget_warnings`)
        // engages and we don't accidentally stop at ~4.29B iterations
        // or emit spurious utilization-based warnings.
        let max_iterations = if self.max_turns == u32::MAX {
            usize::MAX
        } else {
            self.max_turns as usize
        };

        AgentLoopConfig {
            max_iterations,
            model: self.model.clone(),
            system_prompt,
            max_tokens: self.max_tokens,
            max_context_tokens: Some(self.context_window_tokens),
            stream_timeout: agent_loop_stream_timeout(),
            auth_token: self.auth_token.clone(),
            // The wire-level `SessionModelOverrides` no longer carries
            // an upstream provider-family hint — proxy routing is the
            // single LLM path. Per-request family hints (used by
            // X-Aura-Upstream-Provider-Family on outbound calls) come
            // from elsewhere if and when they are needed.
            upstream_provider_family: None,
            aura_project_id: self.project_id.clone(),
            aura_agent_id: self.aura_agent_id.clone(),
            aura_session_id: self.aura_session_id.clone(),
            aura_org_id: self.aura_org_id.clone(),
            intent_classifier: self.intent_classifier.clone(),
            intent_classifier_manifest: self.intent_classifier_manifest.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            prompt_cache_retention: self.prompt_cache_retention.clone(),
            ..AgentLoopConfig::default()
        }
    }
}

/// Reasoner reqwest HTTP timeout, in milliseconds, when
/// `AURA_MODEL_TIMEOUT_MS` is unset or unparsable. Mirrors the default
/// in [`aura_reasoner::anthropic::config::AnthropicConfig::from_env`]
/// (300_000ms / 300s) so the two layers stay numerically aligned even
/// when the env var is not set.
const REASONER_DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Safety margin added on top of the reasoner's reqwest timeout when
/// computing the agent-loop outer-guard `stream_timeout`. The outer
/// guard at [`aura_agent::agent_loop::iteration::AgentLoop::call_model`]
/// must be **strictly greater** than the HTTP-layer timeout, otherwise
/// it preempts a still-healthy stream and the user sees a generic
/// `code: "llm_error"` ("Model call timed out after Ns") instead of the
/// typed `ReasonerError` the HTTP layer would have produced (network /
/// 5xx / rate limit / context overflow).
///
/// 30s is large enough to cover scheduler jitter + any drift between
/// the two timer subsystems; small enough that a genuinely deadlocked
/// future still surfaces in well under a minute past the HTTP cap.
const STREAM_TIMEOUT_MARGIN_SECS: u64 = 30;

/// Outer-guard streaming timeout used by the chat-session
/// [`AgentLoopConfig`].
///
/// Reads `AURA_MODEL_TIMEOUT_MS` (the same env var the reasoner reads
/// for its reqwest request timeout — see
/// [`aura_reasoner::anthropic::config::AnthropicConfig::from_env`]) and
/// adds [`STREAM_TIMEOUT_MARGIN_SECS`] so the HTTP layer always wins
/// the timeout race.
///
/// Pinned by the regression tests in `session::tests` to keep the
/// "outer guard ≥ HTTP timeout" invariant the agent-loop module
/// documents at [`AgentLoopConfig::stream_timeout`] from drifting
/// again. The previous hardcoded `Duration::from_secs(180)` violated
/// that invariant and caused long single LLM calls (e.g. a turn
/// emitting several large `update_spec` tool blocks inline) to fire
/// "Model call timed out after 180s" while the upstream stream was
/// still happily delivering tokens.
pub(crate) fn agent_loop_stream_timeout() -> std::time::Duration {
    let reasoner_ms = std::env::var("AURA_MODEL_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(REASONER_DEFAULT_TIMEOUT_MS);
    std::time::Duration::from_millis(reasoner_ms)
        + std::time::Duration::from_secs(STREAM_TIMEOUT_MARGIN_SECS)
}

/// Translate an [`IntentClassifierSpec`] from the wire protocol into the
/// in-process [`IntentClassifier`] plus a `(tool_name, domain)` manifest
/// the agent loop can consume.
///
/// Kept as a free function (rather than an `impl From`) so both sides of
/// the conversion stay obvious at call sites — the spec flattens a
/// `HashMap<String, String>` while the loop expects a stable `Vec` so
/// filters are deterministic.
fn build_intent_classifier(
    spec: IntentClassifierSpec,
) -> (IntentClassifier, Vec<(String, String)>) {
    let IntentClassifierSpec {
        tier1_domains,
        classifier_rules,
        tool_domains,
    } = spec;
    let rules: Vec<(String, Vec<String>)> = classifier_rules
        .into_iter()
        .map(|r| (r.domain, r.keywords))
        .collect();
    let mut manifest: Vec<(String, String)> = tool_domains.into_iter().collect();
    // Stable ordering keeps `build_request` deterministic even though
    // the classifier itself doesn't care about order.
    manifest.sort_by(|a, b| a.0.cmp(&b.0));
    (IntentClassifier::from_rules(tier1_domains, rules), manifest)
}

/// Phase 5: translate the wire `AgentPermissionsWire` into the harness-core
/// `AgentPermissions` used by tools + the kernel policy. Kept here (rather
/// than in `aura-protocol`) so the protocol crate stays decoupled from
/// harness internals — see the module doc on `aura_protocol::SessionInit`.
pub(crate) fn agent_permissions_from_wire(wire: AgentPermissionsWire) -> AgentPermissions {
    let capabilities = wire
        .capabilities
        .into_iter()
        .filter_map(|c| match c {
            CapabilityWire::SpawnAgent => Some(Capability::SpawnAgent),
            CapabilityWire::ControlAgent => Some(Capability::ControlAgent),
            CapabilityWire::ReadAgent => Some(Capability::ReadAgent),
            CapabilityWire::ListAgents => Some(Capability::ListAgents),
            CapabilityWire::ManageOrgMembers => Some(Capability::ManageOrgMembers),
            CapabilityWire::ManageBilling => Some(Capability::ManageBilling),
            CapabilityWire::InvokeProcess => Some(Capability::InvokeProcess),
            CapabilityWire::PostToFeed => Some(Capability::PostToFeed),
            CapabilityWire::GenerateMedia => Some(Capability::GenerateMedia),
            CapabilityWire::ReadProject { id } => Some(Capability::ReadProject { id }),
            CapabilityWire::WriteProject { id } => Some(Capability::WriteProject { id }),
            CapabilityWire::ReadAllProjects => Some(Capability::ReadAllProjects),
            CapabilityWire::WriteAllProjects => Some(Capability::WriteAllProjects),
            // Forward-compat: a newer server can send capability variants
            // this harness build doesn't know yet. Per the protocol doc,
            // drop them silently rather than rejecting the session — the
            // tools that depend on them simply won't be enforceable here.
            CapabilityWire::Unknown => None,
        })
        .collect();
    let permissions = AgentPermissions {
        scope: AgentScope {
            orgs: wire.scope.orgs,
            projects: wire.scope.projects,
            agent_ids: wire.scope.agent_ids,
        },
        capabilities,
    };
    if permissions.capabilities.is_empty() && permissions.scope.is_universe() {
        AgentPermissions::full_access()
    } else {
        permissions
    }
}

fn lexical_normalize(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Map a model identifier to its maximum context window in tokens.
///
/// Mirrors the authoritative values in aura-router's
/// `providers::max_context_tokens` so the harness uses the full window
/// each model supports instead of a blanket 200K cap.
///
/// Model names arrive as-is from `SessionInit.model`. The interface
/// normalises to aura-prefixed aliases (e.g. `aura-gpt-5-5`) which use
/// hyphens, while direct upstream names use dots (`gpt-5.5`). Both
/// forms must resolve correctly, so every OpenAI arm checks both
/// conventions.
pub(crate) fn context_window_for_model(model: &str) -> u64 {
    match model {
        // Anthropic — substring handles bare (claude-opus-4-6) and
        // aura-prefixed (aura-claude-opus-4-7) names.
        m if m.contains("opus-4") => 1_000_000,
        m if m.contains("sonnet-4") => 1_000_000,
        m if m.contains("haiku-4") => 200_000,
        m if m.starts_with("claude") => 200_000,
        // OpenAI GPT 5.x — aliases use hyphens (aura-gpt-5-5), direct
        // names use dots (gpt-5.5). Mini/nano checked before the base
        // variant so "gpt-5-4" doesn't swallow them.
        m if m.contains("gpt-5.5") || m.contains("gpt-5-5") => 1_000_000,
        m if m.contains("gpt-5.4-mini") || m.contains("gpt-5-4-mini")
            || m.contains("gpt-5.4-nano") || m.contains("gpt-5-4-nano") => 400_000,
        m if m.contains("gpt-5.4") || m.contains("gpt-5-4") => 1_050_000,
        // OpenAI GPT 4.x
        m if m.contains("gpt-4.1") => 1_047_576,
        m if m.contains("gpt-4o") || m.contains("gpt-4-turbo") => 128_000,
        // OpenAI reasoning — substring handles aura- prefix (aura-o3).
        m if m.ends_with("-o1") || m.starts_with("o1") => 200_000,
        m if m.contains("-o3") || m.starts_with("o3") => 200_000,
        m if m.contains("-o4") || m.starts_with("o4") => 200_000,
        // DeepSeek
        m if m.contains("deepseek") => 1_000_000,
        // Fireworks OSS
        m if m.contains("kimi") => 262_144,
        // Safe default
        _ => 200_000,
    }
}

#[cfg(test)]
mod context_window_tests {
    use super::context_window_for_model;

    #[test]
    fn anthropic_aura_aliases() {
        assert_eq!(context_window_for_model("aura-claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("aura-claude-opus-4-6"), 1_000_000);
        assert_eq!(
            context_window_for_model("aura-claude-sonnet-4-6"),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("aura-claude-haiku-4-5"),
            200_000
        );
    }

    #[test]
    fn anthropic_bare_names() {
        assert_eq!(context_window_for_model("claude-opus-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 1_000_000);
        assert_eq!(context_window_for_model("claude-haiku-4-5"), 200_000);
        // Older Claude generations fall to the generic claude catch-all.
        assert_eq!(context_window_for_model("claude-3-5-sonnet"), 200_000);
    }

    #[test]
    fn openai_gpt5_aura_aliases() {
        assert_eq!(context_window_for_model("aura-gpt-5-5"), 1_000_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4"), 1_050_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4-mini"), 400_000);
        assert_eq!(context_window_for_model("aura-gpt-5-4-nano"), 400_000);
    }

    #[test]
    fn openai_gpt5_direct_names() {
        assert_eq!(context_window_for_model("gpt-5.5"), 1_000_000);
        assert_eq!(context_window_for_model("gpt-5.4"), 1_050_000);
        assert_eq!(context_window_for_model("gpt-5.4-mini"), 400_000);
        assert_eq!(context_window_for_model("gpt-5.4-nano"), 400_000);
    }

    #[test]
    fn openai_gpt4_and_reasoning() {
        assert_eq!(context_window_for_model("aura-gpt-4.1"), 1_047_576);
        assert_eq!(context_window_for_model("gpt-4.1"), 1_047_576);
        assert_eq!(context_window_for_model("gpt-4o"), 128_000);
        assert_eq!(context_window_for_model("gpt-4-turbo"), 128_000);
        // Reasoning models — both bare and aura-prefixed.
        assert_eq!(context_window_for_model("o3"), 200_000);
        assert_eq!(context_window_for_model("aura-o3"), 200_000);
        assert_eq!(context_window_for_model("o4-mini"), 200_000);
        assert_eq!(context_window_for_model("aura-o4-mini"), 200_000);
        assert_eq!(context_window_for_model("o1"), 200_000);
    }

    #[test]
    fn deepseek_and_fireworks() {
        assert_eq!(
            context_window_for_model("aura-deepseek-v4-pro"),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("aura-deepseek-v4-flash"),
            1_000_000
        );
        assert_eq!(context_window_for_model("deepseek-v4-pro"), 1_000_000);
        assert_eq!(context_window_for_model("aura-kimi-k2-5"), 262_144);
        assert_eq!(context_window_for_model("aura-kimi-k2-6"), 262_144);
    }

    #[test]
    fn unknown_model_gets_safe_default() {
        assert_eq!(context_window_for_model("unknown-model-xyz"), 200_000);
    }
}
