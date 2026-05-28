//! RocksDB-backed memory store.
//!
//! # Key Encoding
//!
//! Each column family uses a composite key prefixed by the agent ID so that
//! prefix iteration can efficiently list all items for a single agent.
//!
//! | CF | Key format | Size (bytes) |
//! |----|------------|--------------|
//! | `memory_facts` | `agent_id (32) ++ fact_id (16)` | 48 |
//! | `memory_events` | `agent_id (32) ++ timestamp_ms_be (8) ++ event_id (16)` | 56 |
//! | `memory_event_index` | `agent_id (32) ++ event_id (16)` | 48 |
//! | `memory_procedures` | `agent_id (32) ++ procedure_id (16)` | 48 |
//!
//! Events are ordered by timestamp within each agent prefix, enabling
//! efficient chronological and reverse-chronological scans.
//!
//! # Atomicity
//!
//! Multi-key mutations (bulk deletes, wipe) use [`WriteBatch`] so that
//! they are applied atomically — no partial state is observable on failure.

use crate::error::MemoryError;
use crate::types::{AgentEvent, Fact, Procedure};
use aura_core::{AgentEventId, AgentId, FactId, ProcedureId};
use aura_store::cf;
use chrono::{DateTime, Utc};
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded, WriteBatch};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Abstraction over the memory store for testability.
///
/// All operations are blocking — callers on async runtimes should wrap
/// calls in `tokio::task::spawn_blocking`.
pub trait MemoryStoreApi: Send + Sync {
    fn put_fact(&self, fact: &Fact) -> Result<(), MemoryError>;
    fn get_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<Fact, MemoryError>;
    fn get_fact_by_key(&self, agent_id: AgentId, key: &str) -> Result<Option<Fact>, MemoryError>;
    fn list_facts(&self, agent_id: AgentId) -> Result<Vec<Fact>, MemoryError>;
    fn touch_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<(), MemoryError>;
    fn delete_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<(), MemoryError>;

    fn put_event(&self, event: &AgentEvent) -> Result<(), MemoryError>;
    fn list_events(&self, agent_id: AgentId, limit: usize) -> Result<Vec<AgentEvent>, MemoryError>;
    fn list_events_since(
        &self,
        agent_id: AgentId,
        since: DateTime<Utc>,
    ) -> Result<Vec<AgentEvent>, MemoryError>;
    fn delete_event_direct(
        &self,
        agent_id: AgentId,
        timestamp: DateTime<Utc>,
        event_id: AgentEventId,
    ) -> Result<(), MemoryError>;
    fn delete_event(&self, agent_id: AgentId, event_id: AgentEventId) -> Result<(), MemoryError>;
    fn delete_events_before(
        &self,
        agent_id: AgentId,
        before: DateTime<Utc>,
    ) -> Result<usize, MemoryError>;

    fn put_procedure(&self, proc: &Procedure) -> Result<(), MemoryError>;
    fn get_procedure(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
    ) -> Result<Procedure, MemoryError>;
    fn list_procedures(&self, agent_id: AgentId) -> Result<Vec<Procedure>, MemoryError>;
    fn delete_procedure(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
    ) -> Result<(), MemoryError>;

    fn delete_all(&self, agent_id: AgentId) -> Result<(), MemoryError>;
    fn stats(&self, agent_id: AgentId) -> Result<MemoryStats, MemoryError>;
}

pub struct MemoryStore {
    db: Arc<DBWithThreadMode<MultiThreaded>>,
}

impl MemoryStore {
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db }
    }

    /// Expose the raw DB handle for callers that need to wrap operations in
    /// `spawn_blocking`.
    #[must_use]
    pub fn db(&self) -> &Arc<DBWithThreadMode<MultiThreaded>> {
        &self.db
    }

    fn cf_handle(&self, name: &str) -> Result<Arc<rocksdb::BoundColumnFamily<'_>>, MemoryError> {
        self.db
            .cf_handle(name)
            .ok_or_else(|| MemoryError::ColumnFamilyNotFound(name.to_string()))
    }

    // === Key encoding ===

    fn fact_key(agent_id: AgentId, fact_id: FactId) -> Vec<u8> {
        let mut key = Vec::with_capacity(48);
        key.extend_from_slice(agent_id.as_bytes());
        key.extend_from_slice(fact_id.as_bytes());
        key
    }

    fn event_key(agent_id: AgentId, timestamp: DateTime<Utc>, event_id: AgentEventId) -> Vec<u8> {
        let mut key = Vec::with_capacity(56);
        key.extend_from_slice(agent_id.as_bytes());
        key.extend_from_slice(&timestamp.timestamp_millis().to_be_bytes());
        key.extend_from_slice(event_id.as_bytes());
        key
    }

    fn procedure_key(agent_id: AgentId, procedure_id: ProcedureId) -> Vec<u8> {
        let mut key = Vec::with_capacity(48);
        key.extend_from_slice(agent_id.as_bytes());
        key.extend_from_slice(procedure_id.as_bytes());
        key
    }

    fn event_index_key(agent_id: AgentId, event_id: AgentEventId) -> Vec<u8> {
        let mut key = Vec::with_capacity(48);
        key.extend_from_slice(agent_id.as_bytes());
        key.extend_from_slice(event_id.as_bytes());
        key
    }

    fn agent_prefix(agent_id: AgentId) -> Vec<u8> {
        agent_id.as_bytes().to_vec()
    }

    /// Compute the exclusive upper-bound key for prefix iteration.
    ///
    /// Increments the last non-0xFF byte. When all bytes are 0xFF, appends a
    /// zero byte to form a key that is lexicographically greater than any
    /// valid agent prefix.
    pub(crate) fn agent_prefix_end(agent_id: AgentId) -> Vec<u8> {
        let mut end = agent_id.as_bytes().to_vec();
        for byte in end.iter_mut().rev() {
            if *byte < 0xFF {
                *byte += 1;
                return end;
            }
            *byte = 0;
        }
        end.push(0);
        end
    }

    fn batch_delete_range(
        db: &DBWithThreadMode<MultiThreaded>,
        cf: &Arc<rocksdb::BoundColumnFamily<'_>>,
        prefix: &[u8],
        end: &[u8],
        batch: &mut WriteBatch,
    ) -> Result<(), MemoryError> {
        let iter = db.iterator_cf(cf, IteratorMode::From(prefix, rocksdb::Direction::Forward));
        for item in iter {
            let (k, _) = item?;
            if k.as_ref() >= end {
                break;
            }
            batch.delete_cf(cf, &k);
        }
        Ok(())
    }

    fn count_for_agent(&self, cf_name: &str, agent_id: AgentId) -> Result<usize, MemoryError> {
        let cf = self.cf_handle(cf_name)?;
        let prefix = Self::agent_prefix(agent_id);
        let end = Self::agent_prefix_end(agent_id);
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        let mut count = 0usize;
        for item in iter {
            let (k, _) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            count += 1;
        }
        Ok(count)
    }
}

impl MemoryStoreApi for MemoryStore {
    fn put_fact(&self, fact: &Fact) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let key = Self::fact_key(fact.agent_id, fact.fact_id);
        let value = serde_json::to_vec(fact)?;
        self.db.put_cf(&cf, key, value)?;
        Ok(())
    }

    fn get_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<Fact, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let key = Self::fact_key(agent_id, fact_id);
        match self.db.get_cf(&cf, key)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| MemoryError::Deserialization(e.to_string())),
            None => Err(MemoryError::FactNotFound {
                agent_id: agent_id.to_hex(),
                fact_id: fact_id.to_hex(),
            }),
        }
    }

    fn get_fact_by_key(&self, agent_id: AgentId, key: &str) -> Result<Option<Fact>, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let prefix = Self::agent_prefix(agent_id);
        let end = Self::agent_prefix_end(agent_id);
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        for item in iter {
            let (k, v) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let fact: Fact = serde_json::from_slice(&v)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            if fact.key == key {
                return Ok(Some(fact));
            }
        }
        Ok(None)
    }

    fn list_facts(&self, agent_id: AgentId) -> Result<Vec<Fact>, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let prefix = Self::agent_prefix(agent_id);
        let end = Self::agent_prefix_end(agent_id);
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        let mut facts = Vec::new();
        for item in iter {
            let (k, v) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let fact: Fact = serde_json::from_slice(&v)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            facts.push(fact);
        }
        Ok(facts)
    }

    fn touch_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<(), MemoryError> {
        let mut fact = self.get_fact(agent_id, fact_id)?;
        fact.access_count += 1;
        fact.last_accessed = Utc::now();
        self.put_fact(&fact)
    }

    fn delete_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let key = Self::fact_key(agent_id, fact_id);
        self.db.delete_cf(&cf, key)?;
        Ok(())
    }

    fn put_event(&self, event: &AgentEvent) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
        let key = Self::event_key(event.agent_id, event.timestamp, event.event_id);
        let value = serde_json::to_vec(event)?;
        self.db.put_cf(&cf, key, value)?;

        let idx_cf = self.cf_handle(cf::MEMORY_EVENT_INDEX)?;
        let idx_key = Self::event_index_key(event.agent_id, event.event_id);
        self.db.put_cf(
            &idx_cf,
            idx_key,
            event.timestamp.timestamp_millis().to_be_bytes(),
        )?;
        Ok(())
    }

    fn list_events(&self, agent_id: AgentId, limit: usize) -> Result<Vec<AgentEvent>, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
        let end = Self::agent_prefix_end(agent_id);
        let iter = self
            .db
            .iterator_cf(&cf, IteratorMode::From(&end, rocksdb::Direction::Reverse));
        let prefix = Self::agent_prefix(agent_id);

        let mut events = Vec::new();
        for item in iter {
            let (k, v) = item?;
            if k.len() < prefix.len() || k[..prefix.len()] != *prefix.as_slice() {
                break;
            }
            let event: AgentEvent = serde_json::from_slice(&v)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            events.push(event);
            if events.len() >= limit {
                break;
            }
        }
        Ok(events)
    }

    fn list_events_since(
        &self,
        agent_id: AgentId,
        since: DateTime<Utc>,
    ) -> Result<Vec<AgentEvent>, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
        let start = {
            let mut k = Vec::with_capacity(40);
            k.extend_from_slice(agent_id.as_bytes());
            k.extend_from_slice(&since.timestamp_millis().to_be_bytes());
            k
        };
        let end = Self::agent_prefix_end(agent_id);
        let iter = self
            .db
            .iterator_cf(&cf, IteratorMode::From(&start, rocksdb::Direction::Forward));

        let mut events = Vec::new();
        for item in iter {
            let (k, v) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let event: AgentEvent = serde_json::from_slice(&v)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            events.push(event);
        }
        Ok(events)
    }

    fn delete_event_direct(
        &self,
        agent_id: AgentId,
        timestamp: DateTime<Utc>,
        event_id: AgentEventId,
    ) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
        let key = Self::event_key(agent_id, timestamp, event_id);
        self.db.delete_cf(&cf, key)?;

        let idx_cf = self.cf_handle(cf::MEMORY_EVENT_INDEX)?;
        let idx_key = Self::event_index_key(agent_id, event_id);
        self.db.delete_cf(&idx_cf, idx_key)?;
        Ok(())
    }

    fn delete_event(&self, agent_id: AgentId, event_id: AgentEventId) -> Result<(), MemoryError> {
        let idx_cf = self.cf_handle(cf::MEMORY_EVENT_INDEX)?;
        let idx_key = Self::event_index_key(agent_id, event_id);

        match self.db.get_cf(&idx_cf, &idx_key)? {
            Some(ts_bytes) => {
                let ts_arr: [u8; 8] = <[u8; 8]>::try_from(&ts_bytes[..]).map_err(|_| {
                    MemoryError::Deserialization("invalid timestamp in event index".into())
                })?;
                let ts_millis = i64::from_be_bytes(ts_arr);
                let timestamp =
                    chrono::DateTime::from_timestamp_millis(ts_millis).ok_or_else(|| {
                        MemoryError::Deserialization(
                            "invalid timestamp millis in event index".into(),
                        )
                    })?;

                let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
                let key = Self::event_key(agent_id, timestamp, event_id);
                self.db.delete_cf(&cf, key)?;

                self.db.delete_cf(&idx_cf, idx_key)?;
                Ok(())
            }
            None => Err(MemoryError::EventNotFound {
                agent_id: agent_id.to_hex(),
                event_id: event_id.to_hex(),
            }),
        }
    }

    fn delete_events_before(
        &self,
        agent_id: AgentId,
        before: DateTime<Utc>,
    ) -> Result<usize, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_EVENTS)?;
        let idx_cf = self.cf_handle(cf::MEMORY_EVENT_INDEX)?;
        let prefix = Self::agent_prefix(agent_id);
        let cutoff = {
            let mut k = Vec::with_capacity(40);
            k.extend_from_slice(agent_id.as_bytes());
            k.extend_from_slice(&before.timestamp_millis().to_be_bytes());
            k
        };
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        let mut batch = WriteBatch::default();
        let mut deleted = 0usize;
        for item in iter {
            let (k, _) = item?;
            if k.as_ref() >= cutoff.as_slice() {
                break;
            }
            if k.len() < prefix.len() || k[..prefix.len()] != *prefix.as_slice() {
                break;
            }
            batch.delete_cf(&cf, &k);

            if k.len() >= 56 {
                let event_id_bytes = &k[40..56];
                let mut idx_key = Vec::with_capacity(48);
                idx_key.extend_from_slice(agent_id.as_bytes());
                idx_key.extend_from_slice(event_id_bytes);
                batch.delete_cf(&idx_cf, idx_key);
            }

            deleted += 1;
        }

        if deleted > 0 {
            self.db.write(batch)?;
        }
        Ok(deleted)
    }

    fn put_procedure(&self, proc: &Procedure) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_PROCEDURES)?;
        let key = Self::procedure_key(proc.agent_id, proc.procedure_id);
        let value = serde_json::to_vec(proc)?;
        self.db.put_cf(&cf, key, value)?;
        Ok(())
    }

    fn get_procedure(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
    ) -> Result<Procedure, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_PROCEDURES)?;
        let key = Self::procedure_key(agent_id, procedure_id);
        match self.db.get_cf(&cf, key)? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| MemoryError::Deserialization(e.to_string())),
            None => Err(MemoryError::ProcedureNotFound {
                agent_id: agent_id.to_hex(),
                procedure_id: procedure_id.to_hex(),
            }),
        }
    }

    fn list_procedures(&self, agent_id: AgentId) -> Result<Vec<Procedure>, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_PROCEDURES)?;
        let prefix = Self::agent_prefix(agent_id);
        let end = Self::agent_prefix_end(agent_id);
        let iter = self.db.iterator_cf(
            &cf,
            IteratorMode::From(&prefix, rocksdb::Direction::Forward),
        );

        let mut procs = Vec::new();
        for item in iter {
            let (k, v) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let proc: Procedure = serde_json::from_slice(&v)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            procs.push(proc);
        }
        Ok(procs)
    }

    fn delete_procedure(
        &self,
        agent_id: AgentId,
        procedure_id: ProcedureId,
    ) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_PROCEDURES)?;
        let key = Self::procedure_key(agent_id, procedure_id);
        self.db.delete_cf(&cf, key)?;
        Ok(())
    }

    fn delete_all(&self, agent_id: AgentId) -> Result<(), MemoryError> {
        let cf_facts = self.cf_handle(cf::MEMORY_FACTS)?;
        let cf_events = self.cf_handle(cf::MEMORY_EVENTS)?;
        let cf_procs = self.cf_handle(cf::MEMORY_PROCEDURES)?;
        let cf_idx = self.cf_handle(cf::MEMORY_EVENT_INDEX)?;

        let prefix = Self::agent_prefix(agent_id);
        let end = Self::agent_prefix_end(agent_id);
        let mut batch = WriteBatch::default();

        Self::batch_delete_range(&self.db, &cf_facts, &prefix, &end, &mut batch)?;
        Self::batch_delete_range(&self.db, &cf_events, &prefix, &end, &mut batch)?;
        Self::batch_delete_range(&self.db, &cf_procs, &prefix, &end, &mut batch)?;
        Self::batch_delete_range(&self.db, &cf_idx, &prefix, &end, &mut batch)?;

        self.db.write(batch)?;
        Ok(())
    }

    fn stats(&self, agent_id: AgentId) -> Result<MemoryStats, MemoryError> {
        Ok(MemoryStats {
            facts: self.count_for_agent(cf::MEMORY_FACTS, agent_id)?,
            events: self.count_for_agent(cf::MEMORY_EVENTS, agent_id)?,
            procedures: self.count_for_agent(cf::MEMORY_PROCEDURES, agent_id)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    pub facts: usize,
    pub events: usize,
    pub procedures: usize,
}
