//! Session-state unit tests.
//!
//! These were extracted from `session/mod.rs` in Wave 6 / T3 so the
//! module-root file can stay tiny (declarations + `WsContext` + the
//! `Session` re-export).
//!
//! Phase A: the `SessionInit` first-frame contract was replaced with
//! `POST /v1/run` + `WS /stream/:run_id`. These tests now drive
//! [`Session::apply_chat_runtime_request`] directly with chat-shaped
//! [`RuntimeRequest`] payloads built by [`chat_request`].

use super::state::agent_loop_stream_timeout;
use super::Session;
use aura_compaction::{compact_for_storage, SESSION_TOOL_BLOB_MAX_BYTES};
use aura_core::{AgentPermissions, Capability};
use aura_protocol::{
    AgentCapabilities, AgentIdentity, AgentPermissionsWire, ChatProjectInfoWire, ModelSelection,
    ProjectContext, RuntimeRequest, RuntimeRequestType, WorkspaceLocation,
};
use aura_reasoner::Message;
use std::path::PathBuf;

fn absolute_path(parts: &[&str]) -> PathBuf {
    #[cfg(windows)]
    let mut path = PathBuf::from(r"C:\");
    #[cfg(not(windows))]
    let mut path = PathBuf::from("/");

    for part in parts {
        path.push(part);
    }

    path
}

fn test_session(project_base: Option<PathBuf>) -> Session {
    let tmp = std::env::temp_dir().join("aura-test-session");
    let _ = std::fs::create_dir_all(&tmp);
    let mut s = Session::new(tmp);
    s.project_base = project_base;
    s
}

fn blank_chat_request() -> RuntimeRequest {
    RuntimeRequest {
        r#type: RuntimeRequestType::Chat {
            conversation_messages: Vec::new(),
        },
        agent_identity: AgentIdentity::default(),
        model: ModelSelection::default(),
        workspace: WorkspaceLocation::default(),
        project: None,
        agent_permissions: AgentPermissionsWire::default(),
        tool_permissions: None,
        agent_capabilities: AgentCapabilities::default(),
        auth_jwt: None,
        user_id: "user-test".to_string(),
    }
}

fn chat_request_with_project_path(path: &std::path::Path) -> RuntimeRequest {
    let mut req = blank_chat_request();
    req.workspace.project_path = Some(path.display().to_string());
    req
}

#[test]
fn project_path_allowed_when_no_base() {
    let project_path = absolute_path(&["any", "absolute", "path"]);
    let mut session = test_session(None);
    let req = chat_request_with_project_path(&project_path);
    assert!(session.apply_chat_runtime_request(req).is_ok());
    assert_eq!(session.project_path.unwrap(), project_path);
}

#[test]
fn project_path_allowed_under_base() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = project_base.join("myproject");
    let mut session = test_session(Some(project_base));
    let req = chat_request_with_project_path(&project_path);
    assert!(session.apply_chat_runtime_request(req).is_ok());
}

#[test]
fn project_path_blocked_outside_base() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = absolute_path(&["etc", "passwd"]);
    let mut session = test_session(Some(project_base));
    let req = chat_request_with_project_path(&project_path);
    let result = session.apply_chat_runtime_request(req);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("must be under"));
}

#[test]
fn project_path_blocked_with_traversal() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = project_base.join("..").join("etc").join("passwd");
    let mut session = test_session(Some(project_base));
    let req = chat_request_with_project_path(&project_path);
    let result = session.apply_chat_runtime_request(req);
    assert!(result.is_err());
}

#[test]
fn project_path_rejects_relative() {
    let mut session = test_session(None);
    let req = chat_request_with_project_path(std::path::Path::new("relative/path"));
    let result = session.apply_chat_runtime_request(req);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("absolute"));
}

#[test]
fn apply_chat_runtime_request_builds_intent_classifier_when_spec_present() {
    use aura_protocol::{IntentClassifierRule, IntentClassifierSpec};
    use std::collections::HashMap;

    let mut session = test_session(None);
    let mut tool_domains = HashMap::new();
    tool_domains.insert("list_credits".to_string(), "billing".to_string());
    tool_domains.insert("create_project".to_string(), "project".to_string());

    let spec = IntentClassifierSpec {
        tier1_domains: vec!["project".to_string()],
        classifier_rules: vec![IntentClassifierRule {
            domain: "billing".to_string(),
            keywords: vec!["credit".to_string()],
        }],
        tool_domains,
    };
    let mut req = blank_chat_request();
    req.agent_capabilities.intent_classifier = Some(spec);

    session.apply_chat_runtime_request(req).unwrap();

    let classifier = session
        .intent_classifier
        .as_ref()
        .expect("classifier populated");
    let visible = classifier.visible_domains("please check my credit balance");
    assert!(visible.contains(&"project".to_string()));
    assert!(visible.contains(&"billing".to_string()));

    let manifest = &session.intent_classifier_manifest;
    assert_eq!(manifest.len(), 2);
    assert_eq!(manifest[0].0, "create_project");
    assert_eq!(manifest[1].0, "list_credits");

    let cfg = session.agent_loop_config();
    assert!(cfg.intent_classifier.is_some());
    assert_eq!(cfg.intent_classifier_manifest.len(), 2);
}

#[test]
fn apply_chat_runtime_request_leaves_intent_classifier_none_when_spec_absent() {
    let mut session = test_session(None);
    let req = blank_chat_request();
    session.apply_chat_runtime_request(req).unwrap();
    assert!(session.intent_classifier.is_none());
    assert!(session.intent_classifier_manifest.is_empty());

    let cfg = session.agent_loop_config();
    assert!(cfg.intent_classifier.is_none());
    assert!(cfg.intent_classifier_manifest.is_empty());
}

#[test]
fn apply_chat_runtime_request_applies_full_access_permissions_by_default() {
    let mut session = test_session(None);
    session
        .apply_chat_runtime_request(blank_chat_request())
        .unwrap();
    assert_eq!(session.agent_permissions, AgentPermissions::full_access());
}

#[test]
fn apply_chat_runtime_request_uses_template_id_for_skill_lookup() {
    let mut session = test_session(None);
    let mut req = blank_chat_request();
    req.agent_identity.partition_id = Some("spec-gen-project-123".to_string());
    req.agent_identity.template_id = Some("f74bc868-0a34-4195-9718-bf5ce7f67a55".to_string());

    session.apply_chat_runtime_request(req).unwrap();

    assert_eq!(
        session.skill_agent_id.as_deref(),
        Some("f74bc868-0a34-4195-9718-bf5ce7f67a55")
    );
}

#[test]
fn apply_chat_runtime_request_falls_back_to_partition_id_for_skill_lookup() {
    let mut session = test_session(None);
    let mut req = blank_chat_request();
    req.agent_identity.partition_id = Some("legacy-agent-id".to_string());

    session.apply_chat_runtime_request(req).unwrap();

    assert_eq!(session.skill_agent_id.as_deref(), Some("legacy-agent-id"));
}

/// Phase 2 of the cross-repo `parallel-session-chats` plan: two
/// runtime requests that differ only in the trailing `session` segment
/// of the partition string `"{template}::{instance}::{session}"` must
/// yield **distinct** `Session.agent_id`s.
#[test]
fn apply_chat_runtime_request_partitions_session_id_per_session_segment() {
    use aura_core::AgentId;

    let template_uuid = "f74bc868-0a34-4195-9718-bf5ce7f67a55";
    let instance = "abcdef01-2345-6789-abcd-ef0123456789";

    let mut session_a = test_session(None);
    let mut req_a = blank_chat_request();
    req_a.agent_identity.partition_id = Some(format!("{template_uuid}::{instance}::sess-A"));
    req_a.agent_identity.template_id = Some(template_uuid.to_string());
    req_a.project = Some(ProjectContext {
        project_id: "proj-test".to_string(),
        project_info: None,
        aura_agent_id: Some(template_uuid.to_string()),
        aura_org_id: None,
        aura_session_id: None,
    });
    session_a.apply_chat_runtime_request(req_a).unwrap();

    let mut session_b = test_session(None);
    let mut req_b = blank_chat_request();
    req_b.agent_identity.partition_id = Some(format!("{template_uuid}::{instance}::sess-B"));
    req_b.agent_identity.template_id = Some(template_uuid.to_string());
    req_b.project = Some(ProjectContext {
        project_id: "proj-test".to_string(),
        project_info: None,
        aura_agent_id: Some(template_uuid.to_string()),
        aura_org_id: None,
        aura_session_id: None,
    });
    session_b.apply_chat_runtime_request(req_b).unwrap();

    assert_ne!(
        session_a.agent_id, session_b.agent_id,
        "sessions differing only in the trailing session segment must \
         yield distinct Session.agent_ids so they get distinct record \
         logs and turn locks",
    );

    let template_agent_id = AgentId::from_uuid(
        uuid::Uuid::parse_str(template_uuid).expect("static template uuid is valid"),
    );
    assert_eq!(
        session_a.memory_agent_id(),
        template_agent_id,
        "memory_agent_id must resolve to the template uuid so memory \
         is shared across sessions of the same template+instance",
    );
    assert_eq!(
        session_b.memory_agent_id(),
        template_agent_id,
        "memory_agent_id must resolve to the template uuid so memory \
         is shared across sessions of the same template+instance",
    );
}

#[test]
fn apply_chat_runtime_request_applies_explicit_agent_permissions() {
    use aura_protocol::{AgentPermissionsWire, AgentScopeWire, CapabilityWire};
    let mut session = test_session(None);
    let mut req = blank_chat_request();
    req.agent_permissions = AgentPermissionsWire {
        scope: AgentScopeWire {
            orgs: vec!["org-a".into()],
            ..AgentScopeWire::default()
        },
        capabilities: vec![CapabilityWire::SpawnAgent, CapabilityWire::ReadAgent],
    };
    session.apply_chat_runtime_request(req).unwrap();
    let perms = &session.agent_permissions;
    assert_eq!(perms.scope.orgs, vec!["org-a".to_string()]);
    assert!(perms.capabilities.contains(&Capability::SpawnAgent));
    assert!(perms.capabilities.contains(&Capability::ReadAgent));
}

#[test]
fn apply_chat_runtime_request_applies_ceo_preset_when_wired_explicitly() {
    use aura_protocol::{AgentPermissionsWire, CapabilityWire};
    let mut session = test_session(None);
    let mut req = blank_chat_request();
    req.project = Some(ProjectContext {
        project_id: "proj-test".to_string(),
        project_info: None,
        aura_org_id: Some("org-uuid".into()),
        aura_session_id: None,
        aura_agent_id: None,
    });
    req.agent_permissions = AgentPermissionsWire {
        scope: Default::default(),
        capabilities: vec![
            CapabilityWire::SpawnAgent,
            CapabilityWire::ControlAgent,
            CapabilityWire::ReadAgent,
            CapabilityWire::ListAgents,
            CapabilityWire::ManageOrgMembers,
            CapabilityWire::ManageBilling,
            CapabilityWire::InvokeProcess,
            CapabilityWire::PostToFeed,
            CapabilityWire::GenerateMedia,
            CapabilityWire::ReadAllProjects,
            CapabilityWire::WriteAllProjects,
        ],
    };
    session.apply_chat_runtime_request(req).unwrap();
    assert_eq!(session.agent_permissions, AgentPermissions::ceo_preset());
}

/// Chat-WS migration regression: when ANY typed-field is populated
/// on the runtime request, the session prompt must be built via
/// `SystemPromptBuilder` from those typed inputs.
#[test]
fn apply_chat_runtime_request_typed_fields_path_builds_assembled_prompt() {
    use aura_protocol::AgentPersona;

    let mut session = test_session(None);
    let mut req = blank_chat_request();
    req.agent_identity.persona = Some(AgentPersona {
        name: "Atlas".into(),
        role: "Engineer".into(),
        personality: "Precise and methodical.".into(),
    });
    req.agent_identity.skills = vec!["Rust".into(), "TypeScript".into()];
    req.agent_identity.system_prompt = Some("Use TDD on every change.".into());
    req.project = Some(ProjectContext {
        project_id: "proj-test".to_string(),
        project_info: Some(ChatProjectInfoWire {
            id: "00000000-0000-0000-0000-000000000001".into(),
            name: "Demo".into(),
            description: "A demo project.".into(),
            workspace_root: String::new(),
            build_command: "cargo build".into(),
            test_command: "cargo test".into(),
        }),
        aura_org_id: None,
        aura_session_id: None,
        aura_agent_id: None,
    });

    session.apply_chat_runtime_request(req).unwrap();

    let prompt = &session.system_prompt;

    assert!(
        prompt.contains("<chat_capabilities>"),
        "missing <chat_capabilities>: {prompt}"
    );
    assert!(
        prompt.contains("<agent_identity>"),
        "missing <agent_identity>: {prompt}"
    );
    assert!(
        prompt.contains("name: Atlas"),
        "missing identity name: {prompt}"
    );
    assert!(
        prompt.contains("role: Engineer"),
        "missing identity role: {prompt}"
    );
    assert!(
        prompt.contains("Precise and methodical"),
        "missing personality: {prompt}"
    );
    assert!(
        prompt.contains("<agent_skills>"),
        "missing <agent_skills>: {prompt}"
    );
    assert!(prompt.contains("- Rust"), "missing Rust skill: {prompt}");
    assert!(
        prompt.contains("- TypeScript"),
        "missing TypeScript skill: {prompt}"
    );
    assert!(
        prompt.contains("<agent_system_prompt>"),
        "missing <agent_system_prompt>: {prompt}"
    );
    assert!(
        prompt.contains("Use TDD on every change."),
        "missing operator prompt body: {prompt}"
    );
    assert!(
        prompt.contains("<project_context>"),
        "missing <project_context>: {prompt}"
    );
    assert!(
        prompt.contains("project_id: 00000000-0000-0000-0000-000000000001"),
        "missing project id: {prompt}",
    );
    assert!(
        prompt.contains("project_name: Demo"),
        "missing project name: {prompt}"
    );
    assert!(
        prompt.contains("build_command: cargo build"),
        "missing build command: {prompt}"
    );
    assert!(
        prompt.contains("test_command: cargo test"),
        "missing test command: {prompt}"
    );
}

#[test]
fn truncate_messages_for_storage_caps_oversized_tool_result_text() {
    use aura_reasoner::{ContentBlock, Role, ToolResultContent};
    let big = "Z".repeat(SESSION_TOOL_BLOB_MAX_BYTES + 1_000);
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: ToolResultContent::Text(big.clone()),
            is_error: false,
        }],
    }];
    compact_for_storage(&mut messages);
    match &messages[0].content[0] {
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => {
                assert!(t.len() < SESSION_TOOL_BLOB_MAX_BYTES + 200);
                assert!(t.contains("[truncated"));
            }
            other => panic!("expected Text, got {other:?}"),
        },
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn truncate_messages_for_storage_is_noop_for_small_blobs() {
    use aura_reasoner::{ContentBlock, Role, ToolResultContent};
    let small = "ok".to_string();
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: ToolResultContent::Text(small.clone()),
            is_error: false,
        }],
    }];
    compact_for_storage(&mut messages);
    match &messages[0].content[0] {
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => assert_eq!(t, &small),
            other => panic!("expected Text, got {other:?}"),
        },
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn truncate_messages_for_storage_caps_oversized_tool_result_json() {
    use aura_reasoner::{ContentBlock, Role, ToolResultContent};
    let items: Vec<serde_json::Value> = (0..500)
        .map(|i| serde_json::json!({ "id": format!("agent-{i}"), "pad": "X".repeat(200) }))
        .collect();
    let big = serde_json::Value::Array(items);
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tu_list_agents".into(),
            content: ToolResultContent::Json(big.clone()),
            is_error: false,
        }],
    }];
    compact_for_storage(&mut messages);
    match &messages[0].content[0] {
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => {
                assert!(t.len() < SESSION_TOOL_BLOB_MAX_BYTES + 200);
                assert!(t.contains("[truncated"));
            }
            other => {
                panic!("oversized Json should be collapsed to truncated Text, got {other:?}")
            }
        },
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Regression tests for `agent_loop_stream_timeout`
// ---------------------------------------------------------------------------

/// Serializes the env-mutating tests below. Rust runs unit tests in
/// parallel by default and `std::env::{set_var, remove_var}` are
/// process-global, so concurrent mutation would race.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that snapshots the current value of `AURA_MODEL_TIMEOUT_MS`
/// on construction and restores it on drop. Keeps the global env clean
/// across test runs even if an assertion panics.
struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
}

impl EnvVarGuard {
    fn capture(key: &'static str) -> Self {
        Self {
            key,
            prev: std::env::var(key).ok(),
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn agent_loop_stream_timeout_defaults_when_env_unset() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::capture("AURA_MODEL_TIMEOUT_MS");
    std::env::remove_var("AURA_MODEL_TIMEOUT_MS");

    // Default reasoner timeout (300s) + 30s margin = 330s.
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(330),
    );
}

#[test]
fn agent_loop_stream_timeout_reads_env_in_milliseconds_with_margin() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::capture("AURA_MODEL_TIMEOUT_MS");

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "600000");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(630),
    );

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "  120000  ");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(150),
    );
}

#[test]
fn agent_loop_stream_timeout_falls_back_for_invalid_or_zero_values() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::capture("AURA_MODEL_TIMEOUT_MS");

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(330),
    );

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "not-a-number");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(330),
    );

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "0");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(330),
    );
}
