//! Round-trip tests asserting serde compatibility between
//! aura-core and aura-protocol duplicate types.

use std::collections::HashMap;

/// ToolAuth: serialize from aura-core, deserialize as aura-protocol, and vice versa.
#[test]
fn tool_auth_roundtrip_core_to_protocol() {
    let variants: Vec<aura_core_types::ToolAuth> = vec![
        aura_core_types::ToolAuth::None,
        aura_core_types::ToolAuth::Bearer {
            token: "sk-test-123".into(),
        },
        aura_core_types::ToolAuth::ApiKey {
            header: "X-Api-Key".into(),
            key: "key-456".into(),
        },
        aura_core_types::ToolAuth::Headers {
            headers: {
                let mut m = HashMap::new();
                m.insert("Authorization".into(), "Bearer tok".into());
                m.insert("X-Custom".into(), "val".into());
                m
            },
        },
    ];

    for core_val in &variants {
        let json = serde_json::to_string(core_val).expect("serialize core ToolAuth");
        let proto_val: aura_protocol::ToolAuth =
            serde_json::from_str(&json).expect("deserialize as protocol ToolAuth");
        let back_json = serde_json::to_string(&proto_val).expect("re-serialize protocol ToolAuth");
        let roundtrip: aura_core_types::ToolAuth =
            serde_json::from_str(&back_json).expect("deserialize back to core ToolAuth");
        assert_eq!(core_val, &roundtrip, "ToolAuth roundtrip failed for {json}");
    }
}

#[test]
fn tool_auth_roundtrip_protocol_to_core() {
    let variants: Vec<aura_protocol::ToolAuth> = vec![
        aura_protocol::ToolAuth::None,
        aura_protocol::ToolAuth::Bearer {
            token: "sk-test-789".into(),
        },
        aura_protocol::ToolAuth::ApiKey {
            header: "X-Key".into(),
            key: "abc".into(),
        },
        aura_protocol::ToolAuth::Headers {
            headers: {
                let mut m = HashMap::new();
                m.insert("H1".into(), "V1".into());
                m
            },
        },
    ];

    for proto_val in &variants {
        let json = serde_json::to_string(proto_val).expect("serialize protocol ToolAuth");
        let core_val: aura_core_types::ToolAuth =
            serde_json::from_str(&json).expect("deserialize as core ToolAuth");
        let back_json = serde_json::to_string(&core_val).expect("re-serialize core ToolAuth");
        let roundtrip: aura_protocol::ToolAuth =
            serde_json::from_str(&back_json).expect("deserialize back to protocol ToolAuth");
        assert_eq!(
            proto_val, &roundtrip,
            "ToolAuth roundtrip failed for {json}"
        );
    }
}

#[test]
fn installed_tool_roundtrip_core_to_protocol() {
    let core_tool = aura_core_types::InstalledToolDefinition {
        name: "my_tool".into(),
        description: "A test tool".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        endpoint: "http://localhost:8080/tool".into(),
        auth: aura_core_types::ToolAuth::Bearer {
            token: "tok".into(),
        },
        timeout_ms: Some(5000),
        namespace: Some("ns".into()),
        required_integration: Some(aura_core_types::InstalledToolIntegrationRequirement {
            integration_id: None,
            provider: Some("brave_search".into()),
            kind: Some("workspace_integration".into()),
        }),
        runtime_execution: Some(aura_core_types::InstalledToolRuntimeExecution::AppProvider(
            aura_core_types::InstalledToolRuntimeProviderExecution {
                provider: "brave_search".into(),
                base_url: "https://api.search.brave.com".into(),
                static_headers: HashMap::new(),
                integrations: vec![aura_core_types::InstalledToolRuntimeIntegration {
                    integration_id: "int-1".into(),
                    base_url: None,
                    auth: aura_core_types::InstalledToolRuntimeAuth::Header {
                        name: "X-Subscription-Token".into(),
                        value: "secret".into(),
                    },
                    provider_config: HashMap::new(),
                }],
            },
        )),
        metadata: {
            let mut m = HashMap::new();
            m.insert("key".into(), serde_json::json!("value"));
            m
        },
    };

    let json = serde_json::to_string(&core_tool).expect("serialize core InstalledToolDefinition");
    let proto_tool: aura_protocol::InstalledTool =
        serde_json::from_str(&json).expect("deserialize as protocol InstalledTool");

    assert_eq!(core_tool.name, proto_tool.name);
    assert_eq!(core_tool.description, proto_tool.description);
    assert_eq!(core_tool.endpoint, proto_tool.endpoint);
    assert_eq!(core_tool.timeout_ms, proto_tool.timeout_ms);
    assert_eq!(core_tool.namespace, proto_tool.namespace);
    assert!(proto_tool.runtime_execution.is_some());
    assert_eq!(
        core_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.provider.as_deref()),
        proto_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.provider.as_deref())
    );

    let back_json =
        serde_json::to_string(&proto_tool).expect("re-serialize protocol InstalledTool");
    let roundtrip: aura_core_types::InstalledToolDefinition =
        serde_json::from_str(&back_json).expect("deserialize back to core InstalledToolDefinition");

    assert_eq!(core_tool.name, roundtrip.name);
    assert_eq!(core_tool.description, roundtrip.description);
    assert_eq!(core_tool.endpoint, roundtrip.endpoint);
    assert_eq!(core_tool.auth, roundtrip.auth);
    assert_eq!(core_tool.timeout_ms, roundtrip.timeout_ms);
    assert_eq!(core_tool.namespace, roundtrip.namespace);
    assert!(roundtrip.runtime_execution.is_some());
    assert_eq!(
        core_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.kind.as_deref()),
        roundtrip
            .required_integration
            .as_ref()
            .and_then(|req| req.kind.as_deref())
    );
}

#[test]
fn installed_tool_roundtrip_protocol_to_core() {
    let proto_tool = aura_protocol::InstalledTool {
        name: "proto_tool".into(),
        description: "Protocol tool".into(),
        input_schema: serde_json::json!({"type": "object"}),
        endpoint: "http://example.com/api".into(),
        auth: aura_protocol::ToolAuth::ApiKey {
            header: "X-Key".into(),
            key: "secret".into(),
        },
        timeout_ms: None,
        namespace: None,
        required_integration: Some(aura_protocol::InstalledToolIntegrationRequirement {
            integration_id: None,
            provider: Some("github".into()),
            kind: Some("workspace_integration".into()),
        }),
        runtime_execution: Some(aura_protocol::InstalledToolRuntimeExecution::AppProvider(
            aura_protocol::InstalledToolRuntimeProviderExecution {
                provider: "github".into(),
                base_url: "https://api.github.com".into(),
                static_headers: HashMap::new(),
                integrations: vec![aura_protocol::InstalledToolRuntimeIntegration {
                    integration_id: "int-2".into(),
                    base_url: None,
                    auth: aura_protocol::InstalledToolRuntimeAuth::AuthorizationBearer {
                        token: "secret".into(),
                    },
                    provider_config: HashMap::new(),
                }],
            },
        )),
        metadata: HashMap::new(),
    };

    let json = serde_json::to_string(&proto_tool).expect("serialize protocol InstalledTool");
    let core_tool: aura_core_types::InstalledToolDefinition =
        serde_json::from_str(&json).expect("deserialize as core InstalledToolDefinition");

    assert_eq!(proto_tool.name, core_tool.name);
    assert_eq!(proto_tool.description, core_tool.description);
    assert_eq!(proto_tool.endpoint, core_tool.endpoint);
    assert!(core_tool.runtime_execution.is_some());
    assert_eq!(
        proto_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.provider.as_deref()),
        core_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.provider.as_deref())
    );

    let back_json =
        serde_json::to_string(&core_tool).expect("re-serialize core InstalledToolDefinition");
    let roundtrip: aura_protocol::InstalledTool =
        serde_json::from_str(&back_json).expect("deserialize back to protocol InstalledTool");

    assert_eq!(proto_tool.name, roundtrip.name);
    assert_eq!(proto_tool.endpoint, roundtrip.endpoint);
    assert!(roundtrip.runtime_execution.is_some());
    assert_eq!(
        proto_tool
            .required_integration
            .as_ref()
            .and_then(|req| req.kind.as_deref()),
        roundtrip
            .required_integration
            .as_ref()
            .and_then(|req| req.kind.as_deref())
    );
}

#[test]
fn aura_os_contract_includes_current_additive_wire_fields() {
    let request = aura_protocol::RuntimeRequest {
        r#type: aura_protocol::RuntimeRequestType::TaskRun {
            task_id: "task-1".into(),
            prior_failure: Some("previous assertion failed".into()),
            work_log: vec!["implemented parser".into()],
        },
        agent_identity: aura_protocol::AgentIdentity {
            template_id: Some("template-1".into()),
            partition_id: Some("template-1::instance-1::session-1".into()),
            ..Default::default()
        },
        model: aura_protocol::ModelSelection::default(),
        workspace: aura_protocol::WorkspaceLocation::default(),
        project: Some(aura_protocol::ProjectContext {
            project_id: "project-1".into(),
            aura_org_id: Some("org-1".into()),
            aura_session_id: Some("session-1".into()),
            aura_agent_id: Some("agent-1".into()),
            ..Default::default()
        }),
        agent_permissions: aura_protocol::AgentPermissionsWire::default(),
        tool_permissions: None,
        agent_capabilities: aura_protocol::AgentCapabilities {
            installed_tools: vec![aura_protocol::InstalledTool {
                name: "trusted_search".into(),
                description: "Search through an app provider".into(),
                input_schema: serde_json::json!({"type": "object"}),
                endpoint: "app://provider/search".into(),
                auth: aura_protocol::ToolAuth::None,
                timeout_ms: Some(3_000),
                namespace: Some("search".into()),
                required_integration: Some(aura_protocol::InstalledToolIntegrationRequirement {
                    integration_id: Some("integration-1".into()),
                    provider: Some("brave_search".into()),
                    kind: Some("workspace_integration".into()),
                }),
                runtime_execution: Some(aura_protocol::InstalledToolRuntimeExecution::AppProvider(
                    aura_protocol::InstalledToolRuntimeProviderExecution {
                        provider: "brave_search".into(),
                        base_url: "https://api.search.brave.com".into(),
                        static_headers: HashMap::new(),
                        integrations: vec![aura_protocol::InstalledToolRuntimeIntegration {
                            integration_id: "integration-1".into(),
                            base_url: None,
                            auth: aura_protocol::InstalledToolRuntimeAuth::Header {
                                name: "X-Subscription-Token".into(),
                                value: "secret".into(),
                            },
                            provider_config: HashMap::new(),
                        }],
                    },
                )),
                metadata: HashMap::new(),
            }],
            installed_integrations: vec![aura_protocol::InstalledIntegration {
                integration_id: "integration-1".into(),
                name: "Brave Search".into(),
                provider: "brave_search".into(),
                kind: "workspace_integration".into(),
                metadata: HashMap::new(),
            }],
            intent_classifier: None,
            computer_use: false,
            computer_executor_url: None,
        },
        auth_jwt: Some("jwt".into()),
        user_id: "user-1".into(),
    };

    let request_json = serde_json::to_value(&request).unwrap();
    assert_eq!(request_json["type"]["kind"], "task_run");
    assert_eq!(
        request_json["type"]["params"]["work_log"],
        serde_json::json!(["implemented parser"])
    );
    assert_eq!(
        request_json["project"]["aura_org_id"],
        serde_json::json!("org-1")
    );
    assert_eq!(
        request_json["agent_capabilities"]["installed_tools"][0]["required_integration"]
            ["provider"],
        serde_json::json!("brave_search")
    );
    assert_eq!(
        request_json["agent_capabilities"]["installed_tools"][0]["runtime_execution"]["type"],
        serde_json::json!("app_provider")
    );

    let ready = aura_protocol::OutboundMessage::SessionReady(aura_protocol::SessionReady {
        session_id: "run-1".into(),
        tools: vec![aura_protocol::ToolInfo {
            name: "trusted_search".into(),
            description: "Search through an app provider".into(),
            effective_state: aura_protocol::ToolStateWire::Ask,
        }],
        skills: vec![],
    });
    let ready_json = serde_json::to_value(&ready).unwrap();
    assert_eq!(ready_json["tools"][0]["effective_state"], "ask");

    let end = aura_protocol::OutboundMessage::AssistantMessageEnd(Box::new(
        aura_protocol::AssistantMessageEnd {
            message_id: "msg-1".into(),
            stop_reason: "end_turn".into(),
            usage: aura_protocol::SessionUsage {
                input_tokens: 10,
                output_tokens: 5,
                estimated_context_tokens: 20,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 7,
                cumulative_input_tokens: 100,
                cumulative_output_tokens: 50,
                cumulative_cache_creation_input_tokens: 30,
                cumulative_cache_read_input_tokens: 70,
                context_utilization: 0.25,
                model: "claude-opus-4-7".into(),
                provider: "anthropic".into(),
                context_breakdown: aura_protocol::ContextBreakdown {
                    system_prompt_tokens: 1,
                    tools_tokens: 2,
                    skills_tokens: 3,
                    mcp_tokens: 0,
                    subagents_tokens: 4,
                    conversation_tokens: 10,
                    cache_read_tokens: 7,
                    cache_creation_tokens: 3,
                },
                context_contents: Some(aura_protocol::ContextContents {
                    system_prompt: Some("you are a helpful agent".into()),
                    tools: vec![aura_protocol::ContextSegment {
                        label: "read_file".into(),
                        text: "read_file\n\nRead a file.\n\n{}".into(),
                        tokens: 7,
                    }],
                    skills: vec![],
                    subagents: vec![],
                    mcp: vec![],
                }),
            },
            files_changed: aura_protocol::FilesChanged {
                created: vec!["new.txt".into()],
                modified: vec!["changed.txt".into()],
                deleted: vec![],
                diffs: vec![aura_protocol::FileDiff {
                    path: "changed.txt".into(),
                    lines_added: 2,
                    lines_removed: 1,
                }],
            },
            originating_user_id: Some("origin-user-1".into()),
        },
    ));
    let end_json = serde_json::to_value(&end).unwrap();
    assert_eq!(end_json["usage"]["estimated_context_tokens"], 20);
    assert_eq!(end_json["usage"]["cache_read_input_tokens"], 7);
    assert_eq!(
        end_json["usage"]["context_breakdown"]["cache_creation_tokens"],
        3
    );
    assert_eq!(end_json["files_changed"]["diffs"][0]["lines_added"], 2);
    assert_eq!(end_json["originating_user_id"], "origin-user-1");
    assert_eq!(
        end_json["usage"]["context_contents"]["system_prompt"],
        "you are a helpful agent"
    );
    assert_eq!(
        end_json["usage"]["context_contents"]["tools"][0]["label"],
        "read_file"
    );
    let end_decoded: aura_protocol::OutboundMessage = serde_json::from_value(end_json).unwrap();
    match end_decoded {
        aura_protocol::OutboundMessage::AssistantMessageEnd(end) => {
            let contents = end
                .usage
                .context_contents
                .expect("context_contents present");
            assert_eq!(contents.tools.len(), 1);
            assert_eq!(contents.tools[0].tokens, 7);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    let err = aura_protocol::OutboundMessage::Error(aura_protocol::ErrorMsg {
        code: "agent_stalled".into(),
        message: "Agent made no progress".into(),
        recoverable: true,
        support_id: Some("abc123def456".into()),
    });
    let err_json = serde_json::to_value(&err).unwrap();
    assert_eq!(err_json["support_id"], "abc123def456");
}
