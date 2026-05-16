//! Session-state unit tests.
//!
//! These were extracted from `session/mod.rs` in Wave 6 / T3 so the
//! module-root file can stay tiny (declarations + `WsContext` + the
//! `Session` re-export).

use super::state::{
    agent_loop_stream_timeout, truncate_messages_for_storage, SESSION_TOOL_BLOB_MAX_BYTES,
};
use super::Session;
use aura_core::{AgentPermissions, Capability};
use aura_protocol::{AgentPermissionsWire, SessionInit};
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

fn init_with_project_path(path: &std::path::Path) -> SessionInit {
    SessionInit {
        system_prompt: None,
        model: None,
        max_tokens: None,
        temperature: None,
        max_turns: None,
        installed_tools: None,
        installed_integrations: None,
        workspace: None,
        project_path: Some(path.display().to_string()),
        token: None,
        project_id: None,
        conversation_messages: None,
        aura_agent_id: None,
        aura_session_id: None,
        aura_org_id: None,
        agent_id: None,
        template_agent_id: None,
        user_id: "user-test".to_string(),
        tool_permissions: None,
        provider_overrides: None,
        intent_classifier: None,
        agent_permissions: AgentPermissionsWire::default(),
    }
}

#[test]
fn project_path_allowed_when_no_base() {
    let project_path = absolute_path(&["any", "absolute", "path"]);
    let mut session = test_session(None);
    let init = init_with_project_path(&project_path);
    assert!(session.apply_init(init).is_ok());
    assert_eq!(session.project_path.unwrap(), project_path);
}

#[test]
fn project_path_allowed_under_base() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = project_base.join("myproject");
    let mut session = test_session(Some(project_base));
    let init = init_with_project_path(&project_path);
    assert!(session.apply_init(init).is_ok());
}

#[test]
fn project_path_blocked_outside_base() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = absolute_path(&["etc", "passwd"]);
    let mut session = test_session(Some(project_base));
    let init = init_with_project_path(&project_path);
    let result = session.apply_init(init);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("must be under"));
}

#[test]
fn project_path_blocked_with_traversal() {
    let project_base = absolute_path(&["home", "aura"]);
    let project_path = project_base.join("..").join("etc").join("passwd");
    let mut session = test_session(Some(project_base));
    let init = init_with_project_path(&project_path);
    let result = session.apply_init(init);
    assert!(result.is_err());
}

#[test]
fn project_path_rejects_relative() {
    let mut session = test_session(None);
    let init = init_with_project_path(std::path::Path::new("relative/path"));
    let result = session.apply_init(init);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("absolute"));
}

#[test]
fn apply_init_builds_intent_classifier_when_spec_present() {
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
    let init = SessionInit {
        system_prompt: None,
        model: None,
        max_tokens: None,
        temperature: None,
        max_turns: None,
        installed_tools: None,
        installed_integrations: None,
        workspace: None,
        project_path: None,
        token: None,
        project_id: None,
        conversation_messages: None,
        aura_agent_id: None,
        aura_session_id: None,
        aura_org_id: None,
        agent_id: None,
        template_agent_id: None,
        user_id: "user-test".to_string(),
        tool_permissions: None,
        provider_overrides: None,
        intent_classifier: Some(spec),
        agent_permissions: AgentPermissionsWire::default(),
    };

    session.apply_init(init).unwrap();

    let classifier = session
        .intent_classifier
        .as_ref()
        .expect("classifier populated");
    let visible = classifier.visible_domains("please check my credit balance");
    assert!(visible.contains(&"project".to_string()));
    assert!(visible.contains(&"billing".to_string()));

    let manifest = &session.intent_classifier_manifest;
    assert_eq!(manifest.len(), 2);
    // Manifest is sorted for determinism.
    assert_eq!(manifest[0].0, "create_project");
    assert_eq!(manifest[1].0, "list_credits");

    // Carry through to AgentLoopConfig.
    let cfg = session.agent_loop_config();
    assert!(cfg.intent_classifier.is_some());
    assert_eq!(cfg.intent_classifier_manifest.len(), 2);
}

#[test]
fn apply_init_leaves_intent_classifier_none_when_spec_absent() {
    let mut session = test_session(None);
    let init = SessionInit {
        system_prompt: None,
        model: None,
        max_tokens: None,
        temperature: None,
        max_turns: None,
        installed_tools: None,
        installed_integrations: None,
        workspace: None,
        project_path: None,
        token: None,
        project_id: None,
        conversation_messages: None,
        aura_agent_id: None,
        aura_session_id: None,
        aura_org_id: None,
        agent_id: None,
        template_agent_id: None,
        user_id: "user-test".to_string(),
        tool_permissions: None,
        provider_overrides: None,
        intent_classifier: None,
        agent_permissions: AgentPermissionsWire::default(),
    };
    session.apply_init(init).unwrap();
    assert!(session.intent_classifier.is_none());
    assert!(session.intent_classifier_manifest.is_empty());

    let cfg = session.agent_loop_config();
    assert!(cfg.intent_classifier.is_none());
    assert!(cfg.intent_classifier_manifest.is_empty());
}

fn blank_session_init() -> SessionInit {
    SessionInit {
        system_prompt: None,
        model: None,
        max_tokens: None,
        temperature: None,
        max_turns: None,
        installed_tools: None,
        installed_integrations: None,
        workspace: None,
        project_path: None,
        token: None,
        project_id: None,
        conversation_messages: None,
        aura_agent_id: None,
        aura_session_id: None,
        aura_org_id: None,
        agent_id: None,
        template_agent_id: None,
        user_id: "user-test".to_string(),
        tool_permissions: None,
        provider_overrides: None,
        intent_classifier: None,
        agent_permissions: AgentPermissionsWire::default(),
    }
}

#[test]
fn apply_init_applies_full_access_permissions_by_default() {
    let mut session = test_session(None);
    session.apply_init(blank_session_init()).unwrap();
    assert_eq!(session.agent_permissions, AgentPermissions::full_access());
}

#[test]
fn apply_init_uses_template_agent_id_for_skill_lookup() {
    let mut session = test_session(None);
    let mut init = blank_session_init();
    init.agent_id = Some("spec-gen-project-123".to_string());
    init.template_agent_id = Some("f74bc868-0a34-4195-9718-bf5ce7f67a55".to_string());

    session.apply_init(init).unwrap();

    assert_eq!(
        session.skill_agent_id.as_deref(),
        Some("f74bc868-0a34-4195-9718-bf5ce7f67a55")
    );
}

#[test]
fn apply_init_falls_back_to_agent_id_for_skill_lookup() {
    let mut session = test_session(None);
    let mut init = blank_session_init();
    init.agent_id = Some("legacy-agent-id".to_string());

    session.apply_init(init).unwrap();

    assert_eq!(session.skill_agent_id.as_deref(), Some("legacy-agent-id"));
}

#[test]
fn apply_init_applies_explicit_agent_permissions() {
    use aura_protocol::{AgentPermissionsWire, AgentScopeWire, CapabilityWire};
    let mut session = test_session(None);
    let mut init = blank_session_init();
    init.agent_permissions = AgentPermissionsWire {
        scope: AgentScopeWire {
            orgs: vec!["org-a".into()],
            ..AgentScopeWire::default()
        },
        capabilities: vec![CapabilityWire::SpawnAgent, CapabilityWire::ReadAgent],
    };
    session.apply_init(init).unwrap();
    let perms = &session.agent_permissions;
    assert_eq!(perms.scope.orgs, vec!["org-a".to_string()]);
    assert!(perms.capabilities.contains(&Capability::SpawnAgent));
    assert!(perms.capabilities.contains(&Capability::ReadAgent));
}

#[test]
fn apply_init_applies_ceo_preset_when_wired_explicitly() {
    // Keep this in sync with `AgentPermissions::ceo_preset()`. When
    // new capabilities are added to the preset, extend the wire list
    // below so the assertion still holds.
    use aura_protocol::{AgentPermissionsWire, CapabilityWire};
    let mut session = test_session(None);
    let mut init = blank_session_init();
    init.aura_org_id = Some("org-uuid".into());
    init.agent_permissions = AgentPermissionsWire {
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
    session.apply_init(init).unwrap();
    assert_eq!(session.agent_permissions, AgentPermissions::ceo_preset());
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
    truncate_messages_for_storage(&mut messages);
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
    truncate_messages_for_storage(&mut messages);
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
    truncate_messages_for_storage(&mut messages);
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
//
// The chat-session `AgentLoopConfig::stream_timeout` previously hard-coded
// 180s, which was strictly less than the reasoner's reqwest HTTP timeout
// (`AURA_MODEL_TIMEOUT_MS`, default 300_000ms). The outer guard at
// `aura_agent::agent_loop::iteration::AgentLoop::call_model` therefore
// preempted long-but-healthy LLM streams (e.g. a turn emitting several
// large `update_spec` tool blocks inline) and fired
// `LlmCallError::Fatal("Model call timed out after 180s")`, surfaced to
// clients as `code: "llm_error"`.
//
// These tests pin the new helper's invariant — outer guard MUST be
// strictly greater than the reasoner's HTTP timeout — and exercise the
// `AURA_MODEL_TIMEOUT_MS` override path.
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

    // 600_000ms + 30s margin = 630s.
    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "600000");
    assert_eq!(
        agent_loop_stream_timeout(),
        std::time::Duration::from_secs(630),
    );

    // Whitespace / trailing characters are tolerated by the trim+parse path.
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

    // Empty / non-numeric / zero must NOT collapse the outer guard to
    // the margin alone — they fall back to the documented default so
    // a typo can't silently shrink the timeout below the HTTP layer.
    let default = std::time::Duration::from_secs(330);

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "");
    assert_eq!(agent_loop_stream_timeout(), default);

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "nope");
    assert_eq!(agent_loop_stream_timeout(), default);

    std::env::set_var("AURA_MODEL_TIMEOUT_MS", "0");
    assert_eq!(agent_loop_stream_timeout(), default);
}

/// Core invariant: the outer-guard timeout used by the agent loop MUST
/// be strictly greater than the reasoner's HTTP request timeout, for
/// every value of `AURA_MODEL_TIMEOUT_MS` (including the unset default
/// and any explicit override). Without this, a still-healthy stream
/// can be preempted by the outer `tokio::time::timeout`, surfacing a
/// generic `code: "llm_error"` instead of the typed `ReasonerError`
/// the HTTP layer would have produced.
#[test]
fn agent_loop_stream_timeout_strictly_exceeds_reasoner_timeout() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _guard = EnvVarGuard::capture("AURA_MODEL_TIMEOUT_MS");

    // Default path (no env set).
    std::env::remove_var("AURA_MODEL_TIMEOUT_MS");
    let outer = agent_loop_stream_timeout();
    let inner = std::time::Duration::from_millis(300_000);
    assert!(
        outer > inner,
        "default outer guard ({outer:?}) must exceed default reasoner timeout ({inner:?})"
    );

    // Several explicit overrides — match the reasoner config's parser.
    for ms in [60_000_u64, 180_000, 300_000, 600_000, 900_000] {
        std::env::set_var("AURA_MODEL_TIMEOUT_MS", ms.to_string());
        let outer = agent_loop_stream_timeout();
        let inner = std::time::Duration::from_millis(ms);
        assert!(
            outer > inner,
            "outer guard ({outer:?}) must exceed reasoner timeout ({inner:?}) for AURA_MODEL_TIMEOUT_MS={ms}"
        );
    }
}
