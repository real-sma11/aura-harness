use crate::retrieval::{MemoryQueryContext, MemoryRetriever, RetrievalConfig};
use crate::store::{MemoryStore, MemoryStoreApi};
use crate::types::{
    AgentEvent, Fact, FactSource, MemoryAccessContext, MemoryContinuity, MemoryRetrievalMode,
    MemoryScope, MemorySensitivity, MemoryStatus, Procedure,
};
use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
use chrono::{Duration, Utc};
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
use std::collections::HashSet;
use std::sync::Arc;

fn test_db(dir: &std::path::Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    let cfs = vec![
        ColumnFamilyDescriptor::new("record", Options::default()),
        ColumnFamilyDescriptor::new("agent_meta", Options::default()),
        ColumnFamilyDescriptor::new("inbox", Options::default()),
        ColumnFamilyDescriptor::new("memory_facts", Options::default()),
        ColumnFamilyDescriptor::new("memory_events", Options::default()),
        ColumnFamilyDescriptor::new("memory_procedures", Options::default()),
        ColumnFamilyDescriptor::new("memory_event_index", Options::default()),
        ColumnFamilyDescriptor::new("memory_config", Options::default()),
        ColumnFamilyDescriptor::new("agent_skills", Options::default()),
    ];
    Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
}

fn make_fact(agent_id: AgentId, key: &str, val: &str, importance: f32, confidence: f32) -> Fact {
    let now = Utc::now();
    Fact {
        fact_id: FactId::generate(),
        agent_id,
        key: key.to_string(),
        value: serde_json::Value::String(val.to_string()),
        confidence,
        source: FactSource::Extracted,
        importance,
        access_count: 0,
        last_accessed: now,
        created_at: now,
        updated_at: now,
        continuity: MemoryContinuity::default(),
    }
}

fn make_event(
    agent_id: AgentId,
    event_type: &str,
    summary: &str,
    ts: chrono::DateTime<Utc>,
) -> AgentEvent {
    AgentEvent {
        event_id: AgentEventId::generate(),
        agent_id,
        event_type: event_type.to_string(),
        summary: summary.to_string(),
        metadata: serde_json::Value::Null,
        importance: 0.6,
        access_count: 0,
        last_accessed: ts,
        timestamp: ts,
        continuity: MemoryContinuity::default(),
    }
}

fn make_procedure(agent_id: AgentId, name: &str, steps: &[&str]) -> Procedure {
    let now = Utc::now();
    Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id,
        name: name.to_string(),
        trigger: "test trigger".to_string(),
        steps: steps.iter().map(|s| (*s).to_string()).collect(),
        context_constraints: serde_json::Value::Null,
        success_rate: 0.8,
        execution_count: 5,
        last_used: now,
        created_at: now,
        updated_at: now,
        skill_name: None,
        skill_relevance: None,
        continuity: MemoryContinuity::default(),
    }
}

// ====================================================================
// TEST-3: MemoryRetriever integration tests
// ====================================================================

#[tokio::test]
async fn retrieve_empty_store_returns_empty_packet() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let retriever = MemoryRetriever::new(store, RetrievalConfig::default());
    let agent = AgentId::generate();

    let packet = retriever.retrieve(agent).await.unwrap();
    assert!(packet.facts.is_empty());
    assert!(packet.events.is_empty());
    assert!(packet.procedures.is_empty());
    assert!(packet.is_empty());
}

#[tokio::test]
async fn retrieve_returns_facts_events_procedures() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();
    let now = Utc::now();

    store
        .put_fact(&make_fact(agent, "lang", "Rust", 0.8, 0.9))
        .unwrap();
    store
        .put_event(&make_event(agent, "task", "did stuff", now))
        .unwrap();
    store
        .put_procedure(&make_procedure(agent, "deploy", &["build", "push"]))
        .unwrap();

    let retriever = MemoryRetriever::new(store, RetrievalConfig::default());
    let packet = retriever.retrieve(agent).await.unwrap();

    assert_eq!(packet.facts.len(), 1);
    assert_eq!(packet.events.len(), 1);
    assert_eq!(packet.procedures.len(), 1);
    assert_eq!(packet.facts[0].key, "lang");
}

#[tokio::test]
async fn retrieve_filters_low_confidence_facts() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    store
        .put_fact(&make_fact(agent, "low1", "v", 0.5, 0.1))
        .unwrap();
    store
        .put_fact(&make_fact(agent, "low2", "v", 0.5, 0.2))
        .unwrap();
    store
        .put_fact(&make_fact(agent, "high", "v", 0.5, 0.9))
        .unwrap();

    let config = RetrievalConfig {
        min_confidence: 0.3,
        ..RetrievalConfig::default()
    };
    let retriever = MemoryRetriever::new(store, config);
    let packet = retriever.retrieve(agent).await.unwrap();

    assert_eq!(packet.facts.len(), 1);
    assert_eq!(packet.facts[0].key, "high");
}

#[tokio::test]
async fn retrieve_respects_max_facts() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    for i in 0..30 {
        store
            .put_fact(&make_fact(agent, &format!("fact_{i}"), "v", 0.5, 0.9))
            .unwrap();
    }

    let config = RetrievalConfig {
        max_facts: 5,
        token_budget: 100_000,
        ..RetrievalConfig::default()
    };
    let retriever = MemoryRetriever::new(store, config);
    let packet = retriever.retrieve(agent).await.unwrap();

    assert_eq!(packet.facts.len(), 5);
}

#[tokio::test]
async fn retrieve_respects_max_events() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();
    let now = Utc::now();

    for i in 0..20 {
        store
            .put_event(&make_event(
                agent,
                "task",
                &format!("event {i}"),
                now + Duration::seconds(i),
            ))
            .unwrap();
    }

    let config = RetrievalConfig {
        max_events: 3,
        token_budget: 100_000,
        ..RetrievalConfig::default()
    };
    let retriever = MemoryRetriever::new(store, config);
    let packet = retriever.retrieve(agent).await.unwrap();

    assert_eq!(packet.events.len(), 3);
}

#[tokio::test]
async fn retrieve_sorts_by_salience() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    store
        .put_fact(&make_fact(agent, "low_importance", "v", 0.1, 0.9))
        .unwrap();
    store
        .put_fact(&make_fact(agent, "high_importance", "v", 0.95, 0.9))
        .unwrap();
    store
        .put_fact(&make_fact(agent, "mid_importance", "v", 0.5, 0.9))
        .unwrap();

    let config = RetrievalConfig {
        token_budget: 100_000,
        ..RetrievalConfig::default()
    };
    let retriever = MemoryRetriever::new(store, config);
    let packet = retriever.retrieve(agent).await.unwrap();

    assert_eq!(packet.facts.len(), 3);
    assert_eq!(packet.facts[0].key, "high_importance");
    assert_eq!(packet.facts[1].key, "mid_importance");
    assert_eq!(packet.facts[2].key, "low_importance");
}

#[tokio::test]
async fn retrieve_updates_access_count() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let fact = make_fact(agent, "tracked", "v", 0.8, 0.9);
    let fact_id = fact.fact_id;
    store.put_fact(&fact).unwrap();

    assert_eq!(store.get_fact(agent, fact_id).unwrap().access_count, 0);

    let retriever = MemoryRetriever::new(Arc::clone(&store), RetrievalConfig::default());
    let packet = retriever.retrieve(agent).await.unwrap();
    assert_eq!(packet.facts.len(), 1);

    let updated = store.get_fact(agent, fact_id).unwrap();
    assert_eq!(updated.access_count, 1);
}

#[tokio::test]
async fn retrieve_token_budget_limits_output() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    // Each fact costs roughly (4 + key_len + 18 + val_len) / 4 tokens.
    // With long values this adds up quickly.
    for i in 0..20 {
        let long_val = "x".repeat(200);
        store
            .put_fact(&make_fact(agent, &format!("fact_{i}"), &long_val, 0.8, 0.9))
            .unwrap();
    }

    let config = RetrievalConfig {
        max_facts: 20,
        token_budget: 50,
        ..RetrievalConfig::default()
    };
    let retriever = MemoryRetriever::new(store, config);
    let packet = retriever.retrieve(agent).await.unwrap();

    // With a 50-token budget, only a fraction of the 20 facts should fit.
    assert!(packet.facts.len() < 20);
    // A ~200-char value → ~55+ tokens per fact, so the budget of 50 should
    // allow at most 0–1 facts.
    assert!(packet.facts.len() <= 1);
}

#[tokio::test]
async fn query_aware_retrieval_prefers_request_relevance_over_raw_salience() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    store
        .put_fact(&make_fact(
            agent,
            "test_command",
            "cargo test --workspace",
            0.35,
            0.9,
        ))
        .unwrap();
    store
        .put_fact(&make_fact(
            agent,
            "favorite_color",
            "cobalt blue",
            1.0,
            0.99,
        ))
        .unwrap();

    let retriever = MemoryRetriever::new(
        store,
        RetrievalConfig {
            max_facts: 1,
            token_budget: 1_000,
            ..RetrievalConfig::default()
        },
    );
    let packet = retriever
        .retrieve_with_query(
            agent,
            MemoryQueryContext {
                text: "Run the project test command".into(),
                ..MemoryQueryContext::default()
            },
            MemoryRetrievalMode::QueryAware,
        )
        .await
        .unwrap();

    assert_eq!(packet.facts[0].key, "test_command");
    let trace = packet.trace.unwrap();
    assert_eq!(trace.candidate_count, 2);
    assert_eq!(trace.selected_count, 1);
    assert!(trace.query_aware);
    assert_eq!(trace.selections[0].reason, "current_request");
}

#[tokio::test]
async fn query_aware_retrieval_does_not_spend_tokens_on_unrelated_facts() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();
    store
        .put_fact(&make_fact(
            agent,
            "build_command",
            "cargo build --release",
            0.5,
            0.9,
        ))
        .unwrap();
    store
        .put_fact(&make_fact(
            agent,
            "favorite_color",
            "cobalt blue",
            1.0,
            0.99,
        ))
        .unwrap();

    let packet = MemoryRetriever::new(store, RetrievalConfig::default())
        .retrieve_with_query(
            agent,
            MemoryQueryContext {
                text: "What is the build command?".into(),
                ..MemoryQueryContext::default()
            },
            MemoryRetrievalMode::QueryAware,
        )
        .await
        .unwrap();

    assert_eq!(packet.facts.len(), 1);
    assert_eq!(packet.facts[0].key, "build_command");
    assert_eq!(packet.trace.unwrap().selected_count, 1);
}

#[tokio::test]
async fn retrieval_excludes_pending_sensitive_and_unapproved_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let mut active = make_fact(agent, "build_command", "cargo build", 0.7, 0.9);
    active.continuity.status = MemoryStatus::Active;
    store.put_fact(&active).unwrap();

    let mut pending = make_fact(agent, "pending_command", "do not recall", 1.0, 0.99);
    pending.continuity.status = MemoryStatus::Pending;
    store.put_fact(&pending).unwrap();

    let mut sensitive = make_fact(agent, "password", "do not recall", 1.0, 0.99);
    sensitive.continuity.sensitivity = MemorySensitivity::Sensitive;
    store.put_fact(&sensitive).unwrap();

    let mut user_scoped = make_fact(agent, "user_preference", "concise answers", 1.0, 0.99);
    user_scoped.continuity.scope = MemoryScope::User;
    store.put_fact(&user_scoped).unwrap();

    let retriever = MemoryRetriever::new(store, RetrievalConfig::default());
    let packet = retriever
        .retrieve_with_query(
            agent,
            MemoryQueryContext {
                text: "build command pending password preference".into(),
                ..MemoryQueryContext::default()
            },
            MemoryRetrievalMode::QueryAware,
        )
        .await
        .unwrap();

    assert_eq!(packet.facts.len(), 1);
    assert_eq!(packet.facts[0].key, "build_command");
    assert_eq!(packet.trace.unwrap().candidate_count, 4);
}

#[tokio::test]
async fn scoped_retrieval_shares_only_the_intended_three_layers() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent_a = AgentId::generate();
    let agent_b = AgentId::generate();
    let project_x_user_u = MemoryAccessContext {
        project_id: Some("project-x".into()),
        user_id: Some("user-u".into()),
        include_legacy: false,
    };
    let project_y_user_v = MemoryAccessContext {
        project_id: Some("project-y".into()),
        user_id: Some("user-v".into()),
        include_legacy: false,
    };

    let seeds = [
        (
            project_x_user_u.storage_id(agent_a, MemoryScope::Agent),
            MemoryScope::Agent,
            "marker_agent_a_project_x",
        ),
        (
            project_x_user_u.storage_id(agent_b, MemoryScope::Agent),
            MemoryScope::Agent,
            "marker_agent_b_project_x",
        ),
        (
            project_x_user_u.storage_id(agent_a, MemoryScope::Project),
            MemoryScope::Project,
            "marker_project_x_shared",
        ),
        (
            project_x_user_u.storage_id(agent_a, MemoryScope::User),
            MemoryScope::User,
            "marker_user_u_personal",
        ),
        (
            project_y_user_v.storage_id(agent_a, MemoryScope::Project),
            MemoryScope::Project,
            "marker_project_y_shared",
        ),
        (
            project_y_user_v.storage_id(agent_a, MemoryScope::User),
            MemoryScope::User,
            "marker_user_v_personal",
        ),
    ];
    for (partition, scope, key) in seeds {
        let mut fact = make_fact(partition, key, key, 0.9, 0.99);
        fact.continuity.scope = scope;
        store.put_fact(&fact).unwrap();
    }
    store
        .put_fact(&make_fact(
            agent_b,
            "marker_legacy_global_agent",
            "marker_legacy_global_agent",
            0.9,
            0.99,
        ))
        .unwrap();

    let packet = MemoryRetriever::new(
        store,
        RetrievalConfig {
            max_facts: 20,
            token_budget: 10_000,
            ..RetrievalConfig::default()
        },
    )
    .retrieve_with_query(
        agent_b,
        MemoryQueryContext {
            text: "marker".into(),
            allow_user_scope: true,
            allow_project_scope: true,
            access: project_x_user_u,
            ..MemoryQueryContext::default()
        },
        MemoryRetrievalMode::QueryAware,
    )
    .await
    .unwrap();

    let keys: HashSet<_> = packet.facts.iter().map(|fact| fact.key.as_str()).collect();
    assert_eq!(
        keys,
        HashSet::from([
            "marker_agent_b_project_x",
            "marker_project_x_shared",
            "marker_user_u_personal",
        ])
    );
    assert!(packet
        .trace
        .unwrap()
        .selections
        .iter()
        .all(|selection| matches!(
            selection.scope,
            MemoryScope::Agent | MemoryScope::Project | MemoryScope::User
        )));
}
