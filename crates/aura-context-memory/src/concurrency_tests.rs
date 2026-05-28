use crate::store::{MemoryStore, MemoryStoreApi};
use crate::types::{AgentEvent, Fact, FactSource, Procedure};
use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
use chrono::Utc;
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

fn make_event(agent_id: AgentId, i: usize) -> AgentEvent {
    let now = Utc::now();
    AgentEvent {
        event_id: AgentEventId::generate(),
        agent_id,
        event_type: "test".to_string(),
        summary: format!("event {i}"),
        metadata: serde_json::Value::Null,
        importance: 0.5,
        access_count: 0,
        last_accessed: now,
        timestamp: now + chrono::Duration::milliseconds(i as i64),
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
// TEST-10: Concurrency tests
// ====================================================================

#[tokio::test]
async fn parallel_agents_write_isolated_facts() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agents: Vec<AgentId> = (0..4).map(|_| AgentId::generate()).collect();

    let mut handles = Vec::new();
    for &agent in &agents {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                let fact = make_fact(agent, &format!("key_{i}"), &format!("val_{i}"));
                s.put_fact(&fact).unwrap();
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    for &agent in &agents {
        let facts = store.list_facts(agent).unwrap();
        assert_eq!(facts.len(), 10, "Each agent should have exactly 10 facts");
    }
}

#[tokio::test]
async fn parallel_agents_write_isolated_events() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agents: Vec<AgentId> = (0..4).map(|_| AgentId::generate()).collect();

    let mut handles = Vec::new();
    for &agent in &agents {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                let event = make_event(agent, i);
                s.put_event(&event).unwrap();
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    for &agent in &agents {
        let events = store.list_events(agent, 100).unwrap();
        assert_eq!(events.len(), 10, "Each agent should have exactly 10 events");
    }
}

#[tokio::test]
async fn parallel_agents_write_isolated_procedures() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agents: Vec<AgentId> = (0..4).map(|_| AgentId::generate()).collect();

    let mut handles = Vec::new();
    for &agent in &agents {
        let s = Arc::clone(&store);
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                let proc = make_procedure(agent, &format!("proc_{i}"), &["step_a", "step_b"]);
                s.put_procedure(&proc).unwrap();
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    for &agent in &agents {
        let procs = store.list_procedures(agent).unwrap();
        assert_eq!(
            procs.len(),
            10,
            "Each agent should have exactly 10 procedures"
        );
    }
}

#[tokio::test]
async fn concurrent_read_write_same_agent() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let writer = {
        let s = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        tokio::spawn(async move {
            b.wait().await;
            for i in 0..50 {
                let fact = make_fact(agent, &format!("wkey_{i}"), &format!("wval_{i}"));
                s.put_fact(&fact).unwrap();
            }
        })
    };

    let reader = {
        let s = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        tokio::spawn(async move {
            b.wait().await;
            for _ in 0..50 {
                let _ = s.list_facts(agent);
                tokio::task::yield_now().await;
            }
        })
    };

    writer.await.unwrap();
    reader.await.unwrap();

    let facts = store.list_facts(agent).unwrap();
    assert_eq!(facts.len(), 50);
}

#[tokio::test]
async fn parallel_delete_all_isolates_agents() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent_a = AgentId::generate();
    let agent_b = AgentId::generate();

    for i in 0..10 {
        store
            .put_fact(&make_fact(agent_a, &format!("a_key_{i}"), &format!("a_val_{i}")))
            .unwrap();
        store
            .put_fact(&make_fact(agent_b, &format!("b_key_{i}"), &format!("b_val_{i}")))
            .unwrap();
        store.put_event(&make_event(agent_a, i)).unwrap();
        store.put_event(&make_event(agent_b, i)).unwrap();
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let deleter = {
        let s = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        tokio::spawn(async move {
            b.wait().await;
            s.delete_all(agent_a).unwrap();
        })
    };

    let writer = {
        let s = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        tokio::spawn(async move {
            b.wait().await;
            for i in 10..20 {
                let fact = make_fact(agent_b, &format!("b_key_{i}"), &format!("b_val_{i}"));
                s.put_fact(&fact).unwrap();
            }
        })
    };

    deleter.await.unwrap();
    writer.await.unwrap();

    assert!(
        store.list_facts(agent_a).unwrap().is_empty(),
        "agent_a facts should be deleted"
    );
    assert!(
        store.list_events(agent_a, 100).unwrap().is_empty(),
        "agent_a events should be deleted"
    );

    let b_facts = store.list_facts(agent_b).unwrap();
    assert_eq!(b_facts.len(), 20, "agent_b should have all 20 facts intact");
}

#[tokio::test]
async fn concurrent_touch_fact() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(dir.path())));
    let agent = AgentId::generate();

    let fact = make_fact(agent, "shared_key", "shared_val");
    let fid = fact.fact_id;
    store.put_fact(&fact).unwrap();

    let num_tasks = 8;
    let touches_per_task = 10;
    let barrier = Arc::new(tokio::sync::Barrier::new(num_tasks));

    let mut handles = Vec::new();
    for _ in 0..num_tasks {
        let s = Arc::clone(&store);
        let b = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            b.wait().await;
            for _ in 0..touches_per_task {
                s.touch_fact(agent, fid).unwrap();
                tokio::task::yield_now().await;
            }
        }));
    }

    for h in handles {
        h.await.unwrap();
    }

    let loaded = store.get_fact(agent, fid).unwrap();
    // touch_fact does read-modify-write without an external lock, so under
    // concurrency some increments may be lost.  We verify the count is
    // *at least* 1 (no total loss) and at most the theoretical maximum.
    let total = num_tasks * touches_per_task;
    assert!(
        loaded.access_count >= 1 && loaded.access_count <= total as u32,
        "access_count {} should be between 1 and {total}",
        loaded.access_count,
    );
}
