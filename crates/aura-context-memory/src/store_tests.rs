use crate::error::MemoryError;
use crate::store::{MemoryStats, MemoryStore, MemoryStoreApi};
use crate::types::{AgentEvent, Fact, FactSource, Procedure};
use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
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

fn make_fact(agent_id: AgentId, key: &str, val: &str) -> Fact {
    let now = Utc::now();
    Fact {
        fact_id: FactId::generate(),
        agent_id,
        key: key.to_string(),
        value: serde_json::Value::String(val.to_string()),
        confidence: 0.9,
        source: FactSource::Extracted,
        importance: 0.5,
        access_count: 0,
        last_accessed: now,
        created_at: now,
        updated_at: now,
    }
}

fn make_event(agent_id: AgentId, event_type: &str, summary: &str, ts: chrono::DateTime<Utc>) -> AgentEvent {
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
    }
}

// ====================================================================
// 1. MemoryStore CRUD for Facts
// ====================================================================

#[test]
fn put_and_get_fact() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let fact = make_fact(agent, "lang", "Rust");

    store.put_fact(&fact).unwrap();
    let loaded = store.get_fact(agent, fact.fact_id).unwrap();
    assert_eq!(loaded.key, "lang");
    assert_eq!(loaded.value, serde_json::Value::String("Rust".into()));
}

#[test]
fn list_facts_returns_all_for_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();

    let f1 = make_fact(agent, "a", "1");
    let f2 = make_fact(agent, "b", "2");
    store.put_fact(&f1).unwrap();
    store.put_fact(&f2).unwrap();

    let facts = store.list_facts(agent).unwrap();
    assert_eq!(facts.len(), 2);
}

#[test]
fn list_facts_empty_for_unknown_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let unknown = AgentId::generate();

    let facts = store.list_facts(unknown).unwrap();
    assert!(facts.is_empty());
}

#[test]
fn list_facts_isolates_agents() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let a1 = AgentId::generate();
    let a2 = AgentId::generate();

    store.put_fact(&make_fact(a1, "k1", "v1")).unwrap();
    store.put_fact(&make_fact(a2, "k2", "v2")).unwrap();

    assert_eq!(store.list_facts(a1).unwrap().len(), 1);
    assert_eq!(store.list_facts(a2).unwrap().len(), 1);
}

#[test]
fn get_fact_by_key_finds_correct_fact() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();

    let f1 = make_fact(agent, "language", "Rust");
    let f2 = make_fact(agent, "framework", "Axum");
    let f3 = make_fact(agent, "db", "Postgres");
    store.put_fact(&f1).unwrap();
    store.put_fact(&f2).unwrap();
    store.put_fact(&f3).unwrap();

    let found = store.get_fact_by_key(agent, "framework").unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().key, "framework");
}

#[test]
fn get_fact_by_key_returns_none_for_missing() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();

    store.put_fact(&make_fact(agent, "k", "v")).unwrap();
    let found = store.get_fact_by_key(agent, "nonexistent").unwrap();
    assert!(found.is_none());
}

#[test]
fn touch_fact_increments_access_count() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let fact = make_fact(agent, "k", "v");
    let fid = fact.fact_id;
    store.put_fact(&fact).unwrap();

    store.touch_fact(agent, fid).unwrap();
    store.touch_fact(agent, fid).unwrap();

    let loaded = store.get_fact(agent, fid).unwrap();
    assert_eq!(loaded.access_count, 2);
}

#[test]
fn delete_fact_removes_entry() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let fact = make_fact(agent, "k", "v");
    let fid = fact.fact_id;
    store.put_fact(&fact).unwrap();

    store.delete_fact(agent, fid).unwrap();
    let result = store.get_fact(agent, fid);
    assert!(result.is_err());
}

#[test]
fn put_fact_overwrites_existing() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let mut fact = make_fact(agent, "k", "old");
    store.put_fact(&fact).unwrap();

    fact.value = serde_json::Value::String("new".into());
    store.put_fact(&fact).unwrap();

    let loaded = store.get_fact(agent, fact.fact_id).unwrap();
    assert_eq!(loaded.value, serde_json::Value::String("new".into()));
    assert_eq!(store.list_facts(agent).unwrap().len(), 1);
}

// ====================================================================
// 2. MemoryStore CRUD for Events
// ====================================================================

#[test]
fn put_and_list_events() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e1 = make_event(agent, "run", "first", now - Duration::hours(2));
    let e2 = make_event(agent, "run", "second", now - Duration::hours(1));
    let e3 = make_event(agent, "run", "third", now);
    store.put_event(&e1).unwrap();
    store.put_event(&e2).unwrap();
    store.put_event(&e3).unwrap();

    let events = store.list_events(agent, 10).unwrap();
    assert_eq!(events.len(), 3);
}

#[test]
fn list_events_respects_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    for i in 0..5 {
        let e = make_event(agent, "task", &format!("event {i}"), now + Duration::seconds(i));
        store.put_event(&e).unwrap();
    }

    let events = store.list_events(agent, 3).unwrap();
    assert_eq!(events.len(), 3);
}

#[test]
fn list_events_returns_reverse_chronological_order() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e_old = make_event(agent, "task", "oldest", now - Duration::hours(3));
    let e_mid = make_event(agent, "task", "middle", now - Duration::hours(1));
    let e_new = make_event(agent, "task", "newest", now);
    store.put_event(&e_old).unwrap();
    store.put_event(&e_mid).unwrap();
    store.put_event(&e_new).unwrap();

    let events = store.list_events(agent, 10).unwrap();
    assert_eq!(events[0].summary, "newest");
    assert_eq!(events[1].summary, "middle");
    assert_eq!(events[2].summary, "oldest");
}

#[test]
fn list_events_since_filters_by_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e1 = make_event(agent, "t", "old", now - Duration::hours(10));
    let e2 = make_event(agent, "t", "recent", now - Duration::hours(1));
    let e3 = make_event(agent, "t", "newest", now);
    store.put_event(&e1).unwrap();
    store.put_event(&e2).unwrap();
    store.put_event(&e3).unwrap();

    let since = now - Duration::hours(2);
    let events = store.list_events_since(agent, since).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].summary, "recent");
    assert_eq!(events[1].summary, "newest");
}

#[test]
fn delete_event_by_scan() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e1 = make_event(agent, "t", "keep", now - Duration::hours(1));
    let e2 = make_event(agent, "t", "remove", now);
    store.put_event(&e1).unwrap();
    store.put_event(&e2).unwrap();

    store.delete_event(agent, e2.event_id).unwrap();
    let events = store.list_events(agent, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].summary, "keep");
}

#[test]
fn delete_event_direct_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e = make_event(agent, "t", "target", now);
    store.put_event(&e).unwrap();

    store.delete_event_direct(agent, e.timestamp, e.event_id).unwrap();
    let events = store.list_events(agent, 10).unwrap();
    assert!(events.is_empty());
}

#[test]
fn delete_events_before_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e1 = make_event(agent, "t", "very old", now - Duration::hours(48));
    let e2 = make_event(agent, "t", "old", now - Duration::hours(24));
    let e3 = make_event(agent, "t", "recent", now - Duration::hours(1));
    let e4 = make_event(agent, "t", "current", now);
    store.put_event(&e1).unwrap();
    store.put_event(&e2).unwrap();
    store.put_event(&e3).unwrap();
    store.put_event(&e4).unwrap();

    let cutoff = now - Duration::hours(12);
    let deleted = store.delete_events_before(agent, cutoff).unwrap();
    assert_eq!(deleted, 2);

    let remaining = store.list_events(agent, 10).unwrap();
    assert_eq!(remaining.len(), 2);
}

#[test]
fn delete_events_before_none_to_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e = make_event(agent, "t", "future", now + Duration::hours(1));
    store.put_event(&e).unwrap();

    let deleted = store.delete_events_before(agent, now).unwrap();
    assert_eq!(deleted, 0);
    assert_eq!(store.list_events(agent, 10).unwrap().len(), 1);
}

// ====================================================================
// 3. MemoryStore CRUD for Procedures
// ====================================================================

#[test]
fn put_and_get_procedure() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let proc = make_procedure(agent, "deploy", &["build", "push", "verify"]);

    store.put_procedure(&proc).unwrap();
    let loaded = store.get_procedure(agent, proc.procedure_id).unwrap();
    assert_eq!(loaded.name, "deploy");
    assert_eq!(loaded.steps.len(), 3);
}

#[test]
fn list_procedures_returns_all_for_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();

    store.put_procedure(&make_procedure(agent, "deploy", &["a"])).unwrap();
    store.put_procedure(&make_procedure(agent, "test", &["b"])).unwrap();

    let procs = store.list_procedures(agent).unwrap();
    assert_eq!(procs.len(), 2);
}

#[test]
fn delete_procedure_removes_entry() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let proc = make_procedure(agent, "deploy", &["a"]);
    let pid = proc.procedure_id;
    store.put_procedure(&proc).unwrap();

    store.delete_procedure(agent, pid).unwrap();
    let result = store.get_procedure(agent, pid);
    assert!(result.is_err());
}

// ====================================================================
// 4. MemoryStore Aggregates
// ====================================================================

#[test]
fn delete_all_removes_facts_events_procedures() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    store.put_fact(&make_fact(agent, "k", "v")).unwrap();
    store.put_event(&make_event(agent, "t", "s", now)).unwrap();
    store.put_procedure(&make_procedure(agent, "p", &["a"])).unwrap();

    store.delete_all(agent).unwrap();

    assert!(store.list_facts(agent).unwrap().is_empty());
    assert!(store.list_events(agent, 100).unwrap().is_empty());
    assert!(store.list_procedures(agent).unwrap().is_empty());
}

#[test]
fn delete_all_does_not_affect_other_agents() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let a1 = AgentId::generate();
    let a2 = AgentId::generate();
    let now = Utc::now();

    store.put_fact(&make_fact(a1, "k1", "v1")).unwrap();
    store.put_fact(&make_fact(a2, "k2", "v2")).unwrap();
    store.put_event(&make_event(a1, "t", "s", now)).unwrap();
    store.put_event(&make_event(a2, "t", "s", now)).unwrap();

    store.delete_all(a1).unwrap();

    assert!(store.list_facts(a1).unwrap().is_empty());
    assert_eq!(store.list_facts(a2).unwrap().len(), 1);
    assert!(store.list_events(a1, 100).unwrap().is_empty());
    assert_eq!(store.list_events(a2, 100).unwrap().len(), 1);
}

#[test]
fn stats_counts_without_deserialization() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    store.put_fact(&make_fact(agent, "f1", "v")).unwrap();
    store.put_fact(&make_fact(agent, "f2", "v")).unwrap();
    store.put_fact(&make_fact(agent, "f3", "v")).unwrap();
    store.put_event(&make_event(agent, "t", "s1", now)).unwrap();
    store.put_event(&make_event(agent, "t", "s2", now + Duration::seconds(1))).unwrap();
    store.put_procedure(&make_procedure(agent, "p", &["a"])).unwrap();

    let stats = store.stats(agent).unwrap();
    assert_eq!(stats.facts, 3);
    assert_eq!(stats.events, 2);
    assert_eq!(stats.procedures, 1);
}

#[test]
fn stats_zero_for_unknown_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();

    let stats = store.stats(agent).unwrap();
    assert_eq!(stats.facts, 0);
    assert_eq!(stats.events, 0);
    assert_eq!(stats.procedures, 0);
}

// ====================================================================
// 5. MemoryStore Error cases
// ====================================================================

#[test]
fn get_fact_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let fid = FactId::generate();

    let err = store.get_fact(agent, fid).unwrap_err();
    assert!(matches!(err, MemoryError::FactNotFound { .. }));
}

#[test]
fn delete_event_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let eid = AgentEventId::generate();

    let err = store.delete_event(agent, eid).unwrap_err();
    assert!(matches!(err, MemoryError::EventNotFound { .. }));
}

#[test]
fn get_procedure_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let pid = ProcedureId::generate();

    let err = store.get_procedure(agent, pid).unwrap_err();
    assert!(matches!(err, MemoryError::ProcedureNotFound { .. }));
}

#[test]
fn touch_fact_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let fid = FactId::generate();

    let err = store.touch_fact(agent, fid).unwrap_err();
    assert!(err.is_not_found());
}

// ====================================================================
// 6. MemoryError is_not_found
// ====================================================================

#[test]
fn is_not_found_fact() {
    let err = MemoryError::FactNotFound {
        agent_id: "a".into(),
        fact_id: "f".into(),
    };
    assert!(err.is_not_found());
}

#[test]
fn is_not_found_event() {
    let err = MemoryError::EventNotFound {
        agent_id: "a".into(),
        event_id: "e".into(),
    };
    assert!(err.is_not_found());
}

#[test]
fn is_not_found_procedure() {
    let err = MemoryError::ProcedureNotFound {
        agent_id: "a".into(),
        procedure_id: "p".into(),
    };
    assert!(err.is_not_found());
}

#[test]
fn is_not_found_false_for_other_variants() {
    let store_err = MemoryError::Store("oops".into());
    assert!(!store_err.is_not_found());

    let ser_err = MemoryError::Serialization("bad".into());
    assert!(!ser_err.is_not_found());

    let de_err = MemoryError::Deserialization("bad".into());
    assert!(!de_err.is_not_found());

    let cf_err = MemoryError::ColumnFamilyNotFound("cf".into());
    assert!(!cf_err.is_not_found());

    let refine_err = MemoryError::Refinement("fail".into());
    assert!(!refine_err.is_not_found());

    let prov_err = MemoryError::Provider("timeout".into());
    assert!(!prov_err.is_not_found());
}

// ====================================================================
// 7. Salience scoring round-trip
// ====================================================================

#[test]
fn salience_scores_are_finite() {
    let agent = AgentId::generate();
    let now = Utc::now();

    let fact = make_fact(agent, "k", "v");
    let event = make_event(agent, "t", "s", now);
    let proc = make_procedure(agent, "p", &["a"]);

    let fs = crate::salience::score_fact(&fact, now);
    let es = crate::salience::score_event(&event, now);
    let ps = crate::salience::score_procedure(&proc, now);

    assert!(fs.is_finite() && fs > 0.0);
    assert!(es.is_finite() && es > 0.0);
    assert!(ps.is_finite() && ps > 0.0);
}

#[test]
fn salience_higher_importance_ranks_higher() {
    let agent = AgentId::generate();
    let now = Utc::now();

    let mut high = make_fact(agent, "k", "v");
    high.importance = 0.95;
    let mut low = make_fact(agent, "k", "v");
    low.importance = 0.05;

    assert!(crate::salience::score_fact(&high, now) > crate::salience::score_fact(&low, now));
}

#[test]
fn salience_recent_event_outscores_old() {
    let agent = AgentId::generate();
    let now = Utc::now();

    let recent = make_event(agent, "t", "s", now);
    let old = make_event(agent, "t", "s", now - Duration::days(90));

    assert!(crate::salience::score_event(&recent, now) > crate::salience::score_event(&old, now));
}

// ====================================================================
// 8. WriteReport and ConsolidationReport serde round-trip
// ====================================================================

#[test]
fn write_report_serde_roundtrip() {
    let report = crate::WriteReport {
        candidates_extracted: 10,
        candidates_refined: 8,
        facts_written: 3,
        facts_updated: 2,
        events_written: 1,
        candidates_dropped: 2,
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: crate::WriteReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.candidates_extracted, 10);
    assert_eq!(parsed.facts_written, 3);
    assert_eq!(parsed.events_written, 1);
}

#[test]
fn consolidation_report_serde_roundtrip() {
    let report = crate::ConsolidationReport {
        facts_merged: 2,
        facts_evolved: 1,
        events_compressed: 5,
        events_deleted: 20,
        facts_forgotten: 3,
        procedures_forgotten: 1,
        insights_created: 2,
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: crate::ConsolidationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.facts_merged, 2);
    assert_eq!(parsed.events_deleted, 20);
    assert_eq!(parsed.insights_created, 2);
}

#[test]
fn write_report_default_is_zeroed() {
    let report = crate::WriteReport::default();
    assert_eq!(report.candidates_extracted, 0);
    assert_eq!(report.facts_written, 0);
}

#[test]
fn consolidation_report_default_is_zeroed() {
    let report = crate::ConsolidationReport::default();
    assert_eq!(report.facts_merged, 0);
    assert_eq!(report.events_compressed, 0);
}

// ====================================================================
// 9. StepSequence serde round-trip
// ====================================================================

#[test]
fn step_sequence_serde_roundtrip() {
    let seq = crate::StepSequence {
        steps: vec!["read".into(), "edit".into(), "build".into()],
        task_hint: Some("refactor the module".into()),
        succeeded: true,
    };
    let json = serde_json::to_string(&seq).unwrap();
    let parsed: crate::StepSequence = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.steps.len(), 3);
    assert_eq!(parsed.task_hint.as_deref(), Some("refactor the module"));
    assert!(parsed.succeeded);
}

#[test]
fn step_sequence_roundtrip_no_hint() {
    let seq = crate::StepSequence {
        steps: vec!["a".into()],
        task_hint: None,
        succeeded: false,
    };
    let json = serde_json::to_string(&seq).unwrap();
    let parsed: crate::StepSequence = serde_json::from_str(&json).unwrap();
    assert!(parsed.task_hint.is_none());
    assert!(!parsed.succeeded);
}

// ====================================================================
// Additional edge-case tests
// ====================================================================

#[test]
fn memory_stats_serde_roundtrip() {
    let stats = MemoryStats {
        facts: 10,
        events: 20,
        procedures: 5,
    };
    let json = serde_json::to_string(&stats).unwrap();
    let parsed: MemoryStats = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.facts, 10);
    assert_eq!(parsed.events, 20);
    assert_eq!(parsed.procedures, 5);
}

#[test]
fn list_events_since_empty_when_all_before() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let e = make_event(agent, "t", "old", now - Duration::days(30));
    store.put_event(&e).unwrap();

    let events = store.list_events_since(agent, now).unwrap();
    assert!(events.is_empty());
}

#[test]
fn delete_event_scan_among_many() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(test_db(dir.path()));
    let agent = AgentId::generate();
    let now = Utc::now();

    let mut target_id = AgentEventId::generate();
    for i in 0..10 {
        let e = make_event(agent, "task", &format!("ev{i}"), now + Duration::seconds(i));
        if i == 5 {
            target_id = e.event_id;
        }
        store.put_event(&e).unwrap();
    }

    store.delete_event(agent, target_id).unwrap();
    let remaining = store.list_events(agent, 100).unwrap();
    assert_eq!(remaining.len(), 9);
    assert!(remaining.iter().all(|e| e.event_id != target_id));
}

// ====================================================================
// TEST-12: agent_prefix_end edge cases
// ====================================================================

#[test]
fn agent_prefix_end_normal_case() {
    let agent = AgentId::new([0u8; 32]);
    let end = MemoryStore::agent_prefix_end(agent);
    let mut expected = [0u8; 32].to_vec();
    *expected.last_mut().unwrap() = 1;
    assert_eq!(end, expected);
}

#[test]
fn agent_prefix_end_all_0xff() {
    let agent = AgentId::new([0xFF; 32]);
    let end = MemoryStore::agent_prefix_end(agent);
    let mut expected = vec![0u8; 32];
    expected.push(0);
    assert_eq!(end, expected);
}

#[test]
fn agent_prefix_end_last_byte_0xff() {
    let mut bytes = [0u8; 32];
    bytes[31] = 0xFF;
    let agent = AgentId::new(bytes);
    let end = MemoryStore::agent_prefix_end(agent);
    let mut expected = bytes.to_vec();
    expected[31] = 0;
    expected[30] = 1;
    assert_eq!(end, expected);
}
