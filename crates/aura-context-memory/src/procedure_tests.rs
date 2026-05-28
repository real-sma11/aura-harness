use crate::procedures::{ProcedureConfig, ProcedureExtractor, StepSequence};
use crate::store::{MemoryStore, MemoryStoreApi};
use crate::types::{AgentEvent, Procedure};
use aura_core::{AgentEventId, AgentId, ProcedureId};
use chrono::{Duration, Utc};
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
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
        ColumnFamilyDescriptor::new("agent_skills", Options::default()),
    ];
    Arc::new(DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&opts, dir, cfs).unwrap())
}

fn make_sequence(steps: &[&str], task_hint: Option<&str>, succeeded: bool) -> StepSequence {
    StepSequence {
        steps: steps.iter().map(|s| (*s).to_string()).collect(),
        task_hint: task_hint.map(String::from),
        succeeded,
    }
}

fn make_event_with_tool_sequence(
    agent_id: AgentId,
    summary: &str,
    tool_sequence: &[&str],
    ts: chrono::DateTime<Utc>,
) -> AgentEvent {
    AgentEvent {
        event_id: AgentEventId::generate(),
        agent_id,
        event_type: "tool_run".to_string(),
        summary: summary.to_string(),
        metadata: serde_json::json!({
            "tool_sequence": tool_sequence
        }),
        importance: 0.6,
        access_count: 0,
        last_accessed: ts,
        timestamp: ts,
    }
}

fn make_procedure(
    agent_id: AgentId,
    name: &str,
    trigger: &str,
    steps: &[&str],
    success_rate: f32,
    execution_count: u32,
) -> Procedure {
    let now = Utc::now();
    Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id,
        name: name.to_string(),
        trigger: trigger.to_string(),
        steps: steps.iter().map(|s| (*s).to_string()).collect(),
        context_constraints: serde_json::Value::Null,
        success_rate,
        execution_count,
        last_used: now,
        created_at: now,
        updated_at: now,
    }
}

// ====================================================================
// TEST-6: ProcedureExtractor integration tests
// ====================================================================

#[test]
fn extract_skips_short_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let config = ProcedureConfig {
        min_steps: 3,
        ..ProcedureConfig::default()
    };
    let extractor = ProcedureExtractor::new(store, config);

    // Only 2 steps — below min_steps of 3.
    let seq = make_sequence(&["read", "write"], Some("short task"), true);
    let result = extractor.extract_from_steps(agent, &seq).unwrap();
    assert!(result.is_none());
}

#[test]
fn extract_creates_procedure_on_recurring_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();
    let now = Utc::now();

    // Store events with matching tool_sequence metadata so the extractor
    // finds prior occurrences. min_occurrences=2, so one prior event
    // plus the current sequence (counted as +1) should trigger promotion.
    store
        .put_event(&make_event_with_tool_sequence(
            agent,
            "first run",
            &["build", "test", "deploy"],
            now - Duration::hours(2),
        ))
        .unwrap();

    let config = ProcedureConfig {
        min_occurrences: 2,
        min_steps: 3,
        max_procedures: 50,
    };
    let extractor = ProcedureExtractor::new(Arc::clone(&store), config);

    let seq = make_sequence(&["build", "test", "deploy"], Some("deploy app"), true);
    let result = extractor.extract_from_steps(agent, &seq).unwrap();
    assert!(result.is_some());

    let proc = result.unwrap();
    assert_eq!(proc.steps, vec!["build", "test", "deploy"]);
    assert_eq!(proc.agent_id, agent);

    // Verify persisted in the store.
    let stored = store.list_procedures(agent).unwrap();
    assert_eq!(stored.len(), 1);
}

#[test]
fn extract_updates_existing_procedure() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let proc = make_procedure(
        agent,
        "deploy",
        "deploy app",
        &["build", "test", "deploy"],
        0.8,
        5,
    );
    store.put_procedure(&proc).unwrap();

    let config = ProcedureConfig::default();
    let extractor = ProcedureExtractor::new(Arc::clone(&store), config);

    let seq = make_sequence(&["build", "test", "deploy"], Some("deploy again"), true);
    let result = extractor.extract_from_steps(agent, &seq).unwrap();
    assert!(result.is_some());

    let updated = result.unwrap();
    assert_eq!(updated.execution_count, 6);
    assert_eq!(updated.procedure_id, proc.procedure_id);
}

#[test]
fn match_procedures_by_keyword() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let proc = make_procedure(
        agent,
        "deploy app",
        "deploy the application to production",
        &["build", "push", "verify"],
        0.9,
        10,
    );
    store.put_procedure(&proc).unwrap();

    let config = ProcedureConfig::default();
    let extractor = ProcedureExtractor::new(store, config);

    let matches = extractor
        .match_procedures(agent, "deploy application now")
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].procedure_id, proc.procedure_id);
}

#[test]
fn match_procedures_returns_empty_for_no_match() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let proc = make_procedure(
        agent,
        "deploy",
        "deploy the application",
        &["build", "push"],
        0.9,
        10,
    );
    store.put_procedure(&proc).unwrap();

    let config = ProcedureConfig::default();
    let extractor = ProcedureExtractor::new(store, config);

    let matches = extractor
        .match_procedures(agent, "unrelated banana sandwich")
        .unwrap();
    assert!(matches.is_empty());
}

#[test]
fn record_feedback_updates_success_rate() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let proc = make_procedure(
        agent,
        "deploy",
        "deploy app",
        &["build", "test", "deploy"],
        0.5,
        3,
    );
    let pid = proc.procedure_id;
    store.put_procedure(&proc).unwrap();

    let config = ProcedureConfig::default();
    let extractor = ProcedureExtractor::new(Arc::clone(&store), config);

    extractor
        .record_feedback(agent, pid, true, None)
        .unwrap();

    let updated = store.get_procedure(agent, pid).unwrap();
    // EMA: new_rate = 0.8 * 0.5 + 0.2 = 0.6
    assert!((updated.success_rate - 0.6).abs() < 1e-6);
    assert_eq!(updated.execution_count, 4);
}

#[test]
fn record_feedback_merges_steps() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let proc = make_procedure(
        agent,
        "deploy",
        "deploy app",
        &["a", "b", "c"],
        0.8,
        5,
    );
    let pid = proc.procedure_id;
    store.put_procedure(&proc).unwrap();

    let config = ProcedureConfig::default();
    let extractor = ProcedureExtractor::new(Arc::clone(&store), config);

    let actual: Vec<String> = vec!["a".into(), "b".into(), "c".into(), "d".into()];
    extractor
        .record_feedback(agent, pid, true, Some(&actual))
        .unwrap();

    let updated = store.get_procedure(agent, pid).unwrap();
    // merge_steps prefers the longer (new) sequence.
    assert_eq!(updated.steps, vec!["a", "b", "c", "d"]);
}

#[test]
fn enforce_capacity_evicts_lowest_value() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();
    let now = Utc::now();

    // Capacity scoring: success_rate * execution_count.
    // Store 3 procedures, capacity=2 → lowest-scored one is evicted.
    let low = Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id: agent,
        name: "low_value".to_string(),
        trigger: "low".to_string(),
        steps: vec!["a".into(), "b".into(), "c".into()],
        context_constraints: serde_json::Value::Null,
        success_rate: 0.1,   // score = 0.1 * 1 = 0.1
        execution_count: 1,
        last_used: now,
        created_at: now,
        updated_at: now,
    };
    let mid = Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id: agent,
        name: "mid_value".to_string(),
        trigger: "mid".to_string(),
        steps: vec!["d".into(), "e".into(), "f".into()],
        context_constraints: serde_json::Value::Null,
        success_rate: 0.5,   // score = 0.5 * 5 = 2.5
        execution_count: 5,
        last_used: now,
        created_at: now,
        updated_at: now,
    };
    let high = Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id: agent,
        name: "high_value".to_string(),
        trigger: "high".to_string(),
        steps: vec!["g".into(), "h".into(), "i".into()],
        context_constraints: serde_json::Value::Null,
        success_rate: 0.9,   // score = 0.9 * 10 = 9.0
        execution_count: 10,
        last_used: now,
        created_at: now,
        updated_at: now,
    };

    store.put_procedure(&low).unwrap();
    store.put_procedure(&mid).unwrap();
    store.put_procedure(&high).unwrap();

    // Seed one prior event so extract_from_steps can promote a new pattern,
    // which triggers enforce_capacity internally.
    store
        .put_event(&AgentEvent {
            event_id: AgentEventId::generate(),
            agent_id: agent,
            event_type: "tool_run".to_string(),
            summary: "prior run".to_string(),
            metadata: serde_json::json!({"tool_sequence": ["x", "y", "z"]}),
            importance: 0.5,
            access_count: 0,
            last_accessed: now - Duration::hours(1),
            timestamp: now - Duration::hours(1),
        })
        .unwrap();

    let config = ProcedureConfig {
        min_occurrences: 2,
        min_steps: 3,
        max_procedures: 2,
    };
    let extractor = ProcedureExtractor::new(Arc::clone(&store), config);

    // This creates a 4th procedure then evicts down to 2.
    let seq = make_sequence(&["x", "y", "z"], Some("new pattern"), true);
    let result = extractor.extract_from_steps(agent, &seq).unwrap();
    assert!(result.is_some());

    let remaining = store.list_procedures(agent).unwrap();
    assert_eq!(remaining.len(), 2);

    // The lowest-value procedure ("low_value", score=0.1) should be gone.
    let names: Vec<&str> = remaining.iter().map(|p| p.name.as_str()).collect();
    assert!(!names.contains(&"low_value"));
}
