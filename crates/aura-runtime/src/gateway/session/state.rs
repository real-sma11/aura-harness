//! Session state management.
//!
//! This file owns the [`Session`] struct plus everything that maintains
//! its per-connection state: `new`, `apply_chat_runtime_request`, the
//! wire→core permission translator, the intent-classifier builder,
//! `AgentLoopConfig` derivation, and the agent loop configuration
//! derived from session state.
//!
//! Phase A note: the chat session bootstrap used to be driven by an
//! `InboundMessage::SessionInit` first WS frame. With the new
//! `POST /v1/run` + `WS /stream/:run_id` flow the chat fields ship
//! on a [`RuntimeRequest`] over HTTP and the session is fully
//! initialized before the WebSocket attaches.

use crate::gateway::session::ToolApprovalBroker;
use aura_agent::AgentLoopConfig;
use aura_context_prompts::{
    default_system_prompt, AgentIdentity as PromptAgentIdentity, ProjectInfo, SystemPromptBuilder,
};
use aura_core_types::{
    AgentId, AgentPermissions, AgentScope, AgentToolPermissions, Capability,
    InstalledIntegrationDefinition, InstalledToolDefinition,
};
use aura_engine::scheduler::AgentIdentity as RuntimeAgentIdentity;
use aura_model_reasoner::{
    Message, ModelProvider, ModelRequestKind, PromptCacheRetention, ThinkingEffort, ToolDefinition,
};
use aura_protocol::{
    AgentPermissionsWire, AgentPersona, CapabilityWire, ChatProjectInfoWire, IntentClassifierSpec,
    ReasoningEffort, RuntimeRequest, RuntimeRequestType, SessionModelOverrides,
};
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
/// purpose.
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
    /// Optional per-session model overrides resolved from the runtime
    /// request.
    pub(crate) provider_overrides: Option<SessionModelOverrides>,
    /// Stable OpenAI-family `prompt_cache_key` resolved from the
    /// request's `provider_overrides` bundle.
    pub(crate) prompt_cache_key: Option<String>,
    /// Optional OpenAI-family `prompt_cache_retention` paired with
    /// `prompt_cache_key`.
    pub(crate) prompt_cache_retention: Option<String>,
    /// Optional concrete provider override built from
    /// `provider_overrides`.
    pub(crate) provider_override: Option<Arc<dyn ModelProvider + Send + Sync>>,
    /// Max tokens per response.
    pub(crate) max_tokens: u32,
    /// Sampling temperature.
    pub(crate) temperature: Option<f32>,
    /// User-selected reasoning-effort tier forwarded from the chat
    /// model picker via `ModelSelection::reasoning_effort`. When set it
    /// hard-pins the agent loop's thinking effort (see
    /// `AgentLoopConfig::user_thinking_effort`).
    pub(crate) user_thinking_effort: Option<aura_model_reasoner::ThinkingEffort>,
    /// Maximum agentic steps per turn.
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
    /// Optional base directory that project_path must reside under
    /// (remote VM mode).
    pub(super) project_base: Option<PathBuf>,
    /// Whether the chat runtime request has been applied.
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
    /// Optional keyword-driven intent classifier that narrows the
    /// visible tool set per turn.
    pub(crate) intent_classifier: Option<Arc<IntentClassifier>>,
    /// `(tool_name, domain)` pairs paired with [`intent_classifier`].
    ///
    /// [`intent_classifier`]: Self::intent_classifier
    pub(crate) intent_classifier_manifest: Vec<(String, String)>,
    /// Agent permissions for this session, derived directly from the
    /// required `RuntimeRequest.agent_permissions` field. Always
    /// applied to the kernel `PolicyConfig` on kernel construction.
    pub(crate) agent_permissions: AgentPermissions,
    /// Originating user id for tool-default resolution and forever
    /// approvals.
    pub(crate) user_id: String,
    /// Optional per-agent tool override for this session.
    pub(crate) tool_permissions: Option<AgentToolPermissions>,
    /// Live approval broker attached to this WebSocket connection.
    pub(crate) tool_approval_broker: Option<Arc<ToolApprovalBroker>>,
    /// Chat-WS migration: set when `apply_chat_runtime_request`
    /// assembled [`Self::system_prompt`] from the typed identity /
    /// project_info fields via [`SystemPromptBuilder`].
    pub(crate) typed_chat_prompt: bool,
    /// Whether this run opted into Anthropic computer-use. Drives both
    /// the `Capability::ComputerUse` injection (tool visibility) and the
    /// live `ComputerTool` registration on the session resolver.
    pub(crate) computer_use: bool,
    /// Base URL of the desktop computer-use executor the `ComputerTool`
    /// forwards actions to. `None` disables forwarding even when
    /// [`Self::computer_use`] is set.
    pub(crate) computer_executor_url: Option<String>,
}

impl Session {
    /// Create a new uninitialized session with defaults.
    pub(super) fn new(default_workspace: PathBuf) -> Self {
        Self {
            session_id: Uuid::new_v4().to_string(),
            agent_id: AgentId::generate(),
            system_prompt: String::new(),
            model: String::new(),
            provider_name: String::new(),
            provider_overrides: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            provider_override: None,
            max_tokens: 16384,
            temperature: None,
            user_thinking_effort: None,
            max_turns: aura_core_types::MAX_TURNS,
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
            typed_chat_prompt: false,
            computer_use: false,
            computer_executor_url: None,
        }
    }

    /// Apply a [`RuntimeRequest`] carrying a [`RuntimeRequestType::Chat`]
    /// variant to configure this chat session.
    ///
    /// Returns `Err(...)` when the request is not a Chat variant
    /// (DevLoop / TaskRun never construct a chat Session) or when
    /// workspace / project_path validation fails.
    pub(crate) fn apply_chat_runtime_request(
        &mut self,
        request: RuntimeRequest,
    ) -> Result<(), String> {
        let RuntimeRequest {
            r#type,
            agent_identity,
            model,
            workspace,
            project,
            agent_permissions,
            tool_permissions,
            agent_capabilities,
            auth_jwt,
            user_id,
        } = request;

        let conversation_messages = match r#type {
            RuntimeRequestType::Chat {
                conversation_messages,
            } => conversation_messages,
            RuntimeRequestType::DevLoop {}
            | RuntimeRequestType::TaskRun { .. }
            | RuntimeRequestType::Council { .. } => {
                return Err(
                    "session apply only accepts RuntimeRequestType::Chat — DevLoop / TaskRun / \
                     Council runs do not open a chat session"
                        .to_string(),
                );
            }
        };

        if user_id.trim().is_empty() {
            return Err("user_id is required".into());
        }

        let project_info_ref = project.as_ref().and_then(|p| p.project_info.as_ref());
        let typed_prompt = build_typed_chat_system_prompt(
            agent_identity.persona.as_ref(),
            &agent_identity.skills,
            agent_identity.system_prompt.as_deref(),
            project_info_ref,
        );
        match typed_prompt {
            Some(prompt) => {
                self.system_prompt = prompt;
                self.typed_chat_prompt = true;
            }
            None => {
                self.typed_chat_prompt = false;
            }
        }
        if let Some(model_id) = model.id {
            self.context_window_tokens = context_window_for_model(&model_id);
            self.model = model_id;
        }
        if let Some(max_tokens) = model.max_tokens {
            self.max_tokens = max_tokens;
        }
        if let Some(temperature) = model.temperature {
            self.temperature = Some(temperature);
        }
        if let Some(effort) = model.reasoning_effort {
            // Map the typed wire tier onto the reasoner's internal
            // effort enum. `None` (field absent) leaves the agent loop
            // on its internal effort heuristic.
            self.user_thinking_effort = Some(thinking_effort_from_wire(effort));
        }
        if let Some(max_turns) = model.max_turns {
            self.max_turns = max_turns;
        }
        self.installed_tools = agent_capabilities
            .installed_tools
            .into_iter()
            .map(aura_protocol::installed_tool_to_core)
            .collect();
        self.installed_integrations = agent_capabilities
            .installed_integrations
            .into_iter()
            .map(aura_protocol::installed_integration_to_core)
            .collect();
        self.computer_use = agent_capabilities.computer_use;
        self.computer_executor_url = agent_capabilities
            .computer_executor_url
            .filter(|u| !u.trim().is_empty());
        if let Some(ws) = workspace.workspace {
            let candidate = PathBuf::from(&ws);
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
        if let Some(ref pp) = workspace.project_path {
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
            if let Some(ref base) = self.project_base {
                let normalized = lexical_normalize(&candidate);
                let normalized_base = lexical_normalize(base);
                if !normalized.starts_with(&normalized_base) {
                    return Err(format!("project_path must be under {}", base.display()));
                }
            }
            self.project_path = Some(candidate);
        }
        if let Some(token) = auth_jwt {
            self.auth_token = Some(token);
        }
        if let Some(partition_id) = agent_identity.partition_id {
            self.skill_agent_id = Some(
                agent_identity
                    .template_id
                    .as_ref()
                    .filter(|id| !id.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| partition_id.clone()),
            );
            self.agent_id = AgentId::from_hex(&partition_id).unwrap_or_else(|_| {
                let hash = blake3::hash(partition_id.as_bytes());
                AgentId::new(*hash.as_bytes())
            });
        } else if let Some(template_id) = agent_identity.template_id {
            if !template_id.trim().is_empty() {
                self.skill_agent_id = Some(template_id);
            }
        }
        self.user_id = user_id;
        self.tool_permissions =
            tool_permissions.map(aura_protocol::agent_tool_permissions_from_wire);

        // Project context (project_id + billing IDs + intent classifier carrier).
        if let Some(project_ctx) = project {
            self.project_id = Some(project_ctx.project_id);
            if let Some(id) = project_ctx.aura_agent_id {
                self.aura_agent_id = Some(id);
            }
            if let Some(id) = project_ctx.aura_session_id {
                self.aura_session_id = Some(id);
            }
            if let Some(id) = project_ctx.aura_org_id {
                self.aura_org_id = Some(id);
            }
        }
        if let Some(provider_overrides) = model.provider_overrides {
            self.prompt_cache_key = provider_overrides.prompt_cache_key.clone();
            self.prompt_cache_retention = provider_overrides.prompt_cache_retention.clone();
            self.provider_overrides = Some(provider_overrides);
        }
        if let Some(spec) = agent_capabilities.intent_classifier {
            let (classifier, manifest) = build_intent_classifier(spec);
            self.intent_classifier = Some(Arc::new(classifier));
            self.intent_classifier_manifest = manifest;
        }

        self.agent_permissions = agent_permissions_from_wire(agent_permissions);
        // Computer-use arrives as a separate boolean (not a wire
        // capability), so synthesize the `ComputerUse` capability into
        // the bundle when the run opted in. This makes the catalog's
        // capability-gated `computer` tool visible + lets the kernel
        // policy gate accept it.
        if self.computer_use
            && !self
                .agent_permissions
                .capabilities
                .iter()
                .any(|c| matches!(c, aura_core_types::Capability::ComputerUse))
        {
            let insert_at = self
                .agent_permissions
                .capabilities
                .iter()
                .position(|c| {
                    matches!(
                        c,
                        aura_core_types::Capability::ReadAllProjects
                            | aura_core_types::Capability::WriteAllProjects
                    )
                })
                .unwrap_or(self.agent_permissions.capabilities.len());
            self.agent_permissions
                .capabilities
                .insert(insert_at, aura_core_types::Capability::ComputerUse);
        }
        for msg in conversation_messages {
            match msg.role.as_str() {
                "user" => self.messages.push(Message::user(&msg.content)),
                "assistant" => self.messages.push(Message::assistant(&msg.content)),
                _ => {}
            }
        }
        self.initialized = true;
        Ok(())
    }

    /// Return a deterministic `AgentId` for memory keying.
    ///
    /// When the session carries an `aura_agent_id` (the aura-os UUID),
    /// derive the `AgentId` from it so memory queries from the UI use
    /// the same key. Falls back to the random session `agent_id`.
    pub(super) fn memory_agent_id(&self) -> AgentId {
        if let Some(ref uuid_str) = self.aura_agent_id {
            if let Ok(uuid) = uuid::Uuid::parse_str(uuid_str) {
                return AgentId::from_uuid(uuid);
            }
        }
        self.agent_id
    }

    /// Snapshot the session's identity for the runtime-scheduler-side
    /// `AgentIdentityRegistry`.
    pub(crate) fn as_runtime_identity(&self) -> RuntimeAgentIdentity {
        let prompt_cache_retention = self
            .prompt_cache_retention
            .as_deref()
            .and_then(parse_session_cache_retention);
        RuntimeAgentIdentity {
            model: self.model.clone(),
            aura_org_id: self.aura_org_id.clone(),
            aura_session_id: self.aura_session_id.clone(),
            aura_agent_id: self.aura_agent_id.clone(),
            aura_project_id: self.project_id.clone(),
            system_prompt: self.resolved_system_prompt(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            prompt_cache_retention,
            request_kind: ModelRequestKind::Chat,
            max_tokens: self.max_tokens,
            max_context_tokens: usize::try_from(self.context_window_tokens).unwrap_or(usize::MAX),
            auth_token: self.auth_token.clone(),
        }
    }

    /// Resolve the system prompt the same way [`Self::agent_loop_config`]
    /// does (typed-chat path skips the legacy `## Workspace` addendum).
    fn resolved_system_prompt(&self) -> String {
        let base_prompt = if self.system_prompt.is_empty() {
            default_system_prompt()
        } else {
            self.system_prompt.clone()
        };
        match (&self.project_path, self.typed_chat_prompt) {
            (Some(pp), false) => format!(
                "{base_prompt}\n\n## Workspace\n\n\
                 Your workspace root is `{}`. All relative file paths are resolved against this directory. \
                 When referring to files, use paths relative to this root.",
                pp.display()
            ),
            _ => base_prompt,
        }
    }

    /// Build an `AgentLoopConfig` from session state.
    pub(super) fn agent_loop_config(&self) -> AgentLoopConfig {
        let base_prompt = if self.system_prompt.is_empty() {
            default_system_prompt()
        } else {
            self.system_prompt.clone()
        };

        let system_prompt = match (&self.project_path, self.typed_chat_prompt) {
            (Some(pp), false) => format!(
                "{base_prompt}\n\n## Workspace\n\n\
                 Your workspace root is `{}`. All relative file paths are resolved against this directory. \
                 When referring to files, use paths relative to this root.",
                pp.display()
            ),
            _ => base_prompt,
        };

        let max_iterations = if self.max_turns == u32::MAX {
            usize::MAX
        } else {
            self.max_turns as usize
        };

        AgentLoopConfig {
            max_iterations,
            system_prompt,
            max_tokens: self.max_tokens,
            max_context_tokens: Some(self.context_window_tokens),
            stream_timeout: agent_loop_stream_timeout(),
            auth_token: self.auth_token.clone(),
            upstream_provider_family: None,
            // Chat picker's thinking-level selection hard-pins effort
            // for the whole turn; `None` keeps the internal taper.
            user_thinking_effort: self.user_thinking_effort,
            aura_project_id: self.project_id.clone(),
            aura_agent_id: self.aura_agent_id.clone(),
            aura_session_id: self.aura_session_id.clone(),
            aura_org_id: self.aura_org_id.clone(),
            intent_classifier: self.intent_classifier.clone(),
            intent_classifier_manifest: self.intent_classifier_manifest.clone(),
            prompt_cache_key: self.prompt_cache_key.clone(),
            prompt_cache_retention: self.prompt_cache_retention.clone(),
            ..AgentLoopConfig::for_agent(self.model.clone())
        }
    }
}

/// Convert the wire-side `prompt_cache_retention` (`"24h"` /
/// `"in_memory"`) into the typed reasoner enum.
fn parse_session_cache_retention(value: &str) -> Option<PromptCacheRetention> {
    match value {
        "24h" => Some(PromptCacheRetention::Hours24),
        "in_memory" => Some(PromptCacheRetention::InMemory),
        _ => None,
    }
}

/// Reasoner reqwest HTTP timeout, in milliseconds, when
/// `AURA_MODEL_TIMEOUT_MS` is unset or unparsable.
const REASONER_DEFAULT_TIMEOUT_MS: u64 = 300_000;

/// Safety margin added on top of the reasoner's reqwest timeout when
/// computing the agent-loop outer-guard `stream_timeout`.
const STREAM_TIMEOUT_MARGIN_SECS: u64 = 30;

/// Outer-guard streaming timeout used by the chat-session
/// [`AgentLoopConfig`].
pub(crate) fn agent_loop_stream_timeout() -> std::time::Duration {
    let reasoner_ms = std::env::var("AURA_MODEL_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(REASONER_DEFAULT_TIMEOUT_MS);
    std::time::Duration::from_millis(reasoner_ms)
        + std::time::Duration::from_secs(STREAM_TIMEOUT_MARGIN_SECS)
}

/// Chat-WS migration helper: assemble the chat-path system prompt
/// from the typed [`RuntimeRequest`] identity / project info fields
/// via [`SystemPromptBuilder`].
///
/// Returns `Some(prompt)` when at least one typed field is populated,
/// and `None` when every typed field is absent / blank.
fn build_typed_chat_system_prompt(
    persona: Option<&AgentPersona>,
    skills: &[String],
    agent_system_prompt: Option<&str>,
    project_wire: Option<&ChatProjectInfoWire>,
) -> Option<String> {
    let persona_populated = persona.is_some_and(|w| !w.is_empty());
    let skills_populated = skills.iter().any(|s| !s.trim().is_empty());
    let agent_prompt_populated = agent_system_prompt.is_some_and(|s| !s.trim().is_empty());
    let project_populated = project_wire.is_some_and(|w| !w.is_empty());

    if !persona_populated && !skills_populated && !agent_prompt_populated && !project_populated {
        return None;
    }

    let identity = persona
        .filter(|w| !w.is_empty())
        .map(|w| PromptAgentIdentity {
            name: w.name.as_str(),
            role: w.role.as_str(),
            personality: w.personality.as_str(),
        });
    let mut builder = SystemPromptBuilder::new()
        .chat_capabilities()
        .agent_identity(identity)
        .agent_skills(skills)
        .agent_system_prompt(agent_system_prompt);

    if let Some(project) = project_wire.filter(|w| !w.is_empty()) {
        let project_info = ProjectInfo {
            project_id: Some(project.id.as_str()).filter(|s| !s.trim().is_empty()),
            name: project.name.as_str(),
            description: project.description.as_str(),
            folder_path: project.workspace_root.as_str(),
            build_command: Some(project.build_command.as_str()).filter(|s| !s.trim().is_empty()),
            test_command: Some(project.test_command.as_str()).filter(|s| !s.trim().is_empty()),
        };
        builder = builder
            .project_context(&project_info)
            .agents_md_from_workspace(project_info.folder_path);
    }

    Some(builder.build())
}

/// Translate an [`IntentClassifierSpec`] from the wire protocol into
/// the in-process [`IntentClassifier`] plus a `(tool_name, domain)`
/// manifest the agent loop can consume.
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
    manifest.sort_by(|a, b| a.0.cmp(&b.0));
    (IntentClassifier::from_rules(tier1_domains, rules), manifest)
}

/// Map the wire [`ReasoningEffort`] tier onto the reasoner's internal
/// [`ThinkingEffort`]. `Minimal`/`Max` are preserved 1:1; the Anthropic
/// budget mapping happens later in the reasoner's request conversion.
fn thinking_effort_from_wire(effort: ReasoningEffort) -> ThinkingEffort {
    match effort {
        ReasoningEffort::Minimal => ThinkingEffort::Minimal,
        ReasoningEffort::Low => ThinkingEffort::Low,
        ReasoningEffort::Medium => ThinkingEffort::Medium,
        ReasoningEffort::High => ThinkingEffort::High,
        ReasoningEffort::Max => ThinkingEffort::Max,
    }
}

/// Translate the wire `AgentPermissionsWire` into the harness-core
/// `AgentPermissions` used by tools + the kernel policy.
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
/// Phase B / Commit 3: relocated to [`aura_engine::context_window_for_model`].
/// The gateway-side session bootstrap continues to call into the
/// engine's helper so the chat WS path and the automaton bridge see
/// identical token windows for the same model id.
pub(crate) use aura_engine::context_window_for_model;

#[cfg(test)]
mod context_window_tests {
    use super::context_window_for_model;

    #[test]
    fn context_window_reexport_resolves_via_engine() {
        // The engine owns the comprehensive table tests; this one
        // smoke test confirms the local re-export still resolves so
        // a future refactor that drops the `pub(crate) use` line
        // breaks loudly here.
        assert_eq!(context_window_for_model("claude-opus-4-7"), 1_000_000);
        assert_eq!(context_window_for_model("unknown-model-xyz"), 200_000);
    }
}
