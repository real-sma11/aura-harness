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
use crate::types::{AgentContinuityConfig, AgentEvent, Fact, MemoryStatus, Procedure};
use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
use aura_store_db::cf;
use aura_store_db::SealCipher;
use chrono::{DateTime, Utc};
use rocksdb::{DBWithThreadMode, IteratorMode, MultiThreaded, WriteBatch};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::sync::Arc;

/// Abstraction over the memory store for testability.
///
/// All operations are blocking — callers on async runtimes should wrap
/// calls in `tokio::task::spawn_blocking`.
pub trait MemoryStoreApi: Send + Sync {
    fn get_continuity_config(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentContinuityConfig, MemoryError>;
    fn put_continuity_config(
        &self,
        agent_id: AgentId,
        config: &AgentContinuityConfig,
    ) -> Result<(), MemoryError>;

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
    /// Optional value-sealing cipher (Swarm TEE phase 5). When `Some`,
    /// fact / event / procedure values are AES-256-GCM sealed at rest;
    /// `None` keeps the legacy plaintext format byte-for-byte. The
    /// event-id → timestamp index values stay plaintext (pure metadata).
    cipher: Option<Arc<SealCipher>>,
}

impl MemoryStore {
    #[must_use]
    pub const fn new(db: Arc<DBWithThreadMode<MultiThreaded>>) -> Self {
        Self { db, cipher: None }
    }

    /// Create a store with optional sealed (encrypted-at-rest) values.
    #[must_use]
    pub const fn with_cipher(
        db: Arc<DBWithThreadMode<MultiThreaded>>,
        cipher: Option<Arc<SealCipher>>,
    ) -> Self {
        Self { db, cipher }
    }

    fn seal_value(&self, plain: Vec<u8>) -> Result<Vec<u8>, MemoryError> {
        match &self.cipher {
            Some(cipher) => cipher
                .seal(&plain)
                .map_err(|e| MemoryError::Deserialization(format!("sealing value: {e}"))),
            None => Ok(plain),
        }
    }

    fn open_value<'a>(&self, bytes: &'a [u8]) -> Result<Cow<'a, [u8]>, MemoryError> {
        match &self.cipher {
            Some(cipher) => cipher
                .open(bytes)
                .map(Cow::Owned)
                .map_err(|e| MemoryError::Deserialization(format!("opening sealed value: {e}"))),
            None => Ok(Cow::Borrowed(bytes)),
        }
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
    fn get_continuity_config(
        &self,
        agent_id: AgentId,
    ) -> Result<AgentContinuityConfig, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_CONFIG)?;
        match self.db.get_cf(&cf, agent_id.as_bytes())? {
            Some(bytes) => serde_json::from_slice(&self.open_value(&bytes)?)
                .map_err(|e| MemoryError::Deserialization(e.to_string())),
            None => Ok(AgentContinuityConfig::default()),
        }
    }

    fn put_continuity_config(
        &self,
        agent_id: AgentId,
        config: &AgentContinuityConfig,
    ) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_CONFIG)?;
        let value = self.seal_value(serde_json::to_vec(config)?)?;
        self.db.put_cf(&cf, agent_id.as_bytes(), value)?;
        Ok(())
    }

    fn put_fact(&self, fact: &Fact) -> Result<(), MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let key = Self::fact_key(fact.agent_id, fact.fact_id);
        let value = self.seal_value(serde_json::to_vec(fact)?)?;
        self.db.put_cf(&cf, key, value)?;
        Ok(())
    }

    fn get_fact(&self, agent_id: AgentId, fact_id: FactId) -> Result<Fact, MemoryError> {
        let cf = self.cf_handle(cf::MEMORY_FACTS)?;
        let key = Self::fact_key(agent_id, fact_id);
        match self.db.get_cf(&cf, key)? {
            Some(bytes) => serde_json::from_slice(&self.open_value(&bytes)?)
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

        let mut fallback = None;
        for item in iter {
            let (k, v) = item?;
            if k.as_ref() >= end.as_slice() {
                break;
            }
            let fact: Fact = serde_json::from_slice(&self.open_value(&v)?)
                .map_err(|e| MemoryError::Deserialization(e.to_string()))?;
            if fact.key == key {
                if fact.continuity.status == MemoryStatus::Active {
                    return Ok(Some(fact));
                }
                fallback = Some(fact);
            }
        }
        Ok(fallback)
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
            let fact: Fact = serde_json::from_slice(&self.open_value(&v)?)
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
        let value = self.seal_value(serde_json::to_vec(event)?)?;
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
            let event: AgentEvent = serde_json::from_slice(&self.open_value(&v)?)
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
            let event: AgentEvent = serde_json::from_slice(&self.open_value(&v)?)
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
        let value = self.seal_value(serde_json::to_vec(proc)?)?;
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
            Some(bytes) => serde_json::from_slice(&self.open_value(&bytes)?)
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
            let proc: Procedure = serde_json::from_slice(&self.open_value(&v)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FactSource;
    use aura_store_db::SealCipher;
    use rocksdb::{ColumnFamilyDescriptor, Options};

    fn test_db(dir: &std::path::Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![
            ColumnFamilyDescriptor::new(cf::MEMORY_FACTS, Options::default()),
            ColumnFamilyDescriptor::new(cf::MEMORY_EVENTS, Options::default()),
            ColumnFamilyDescriptor::new(cf::MEMORY_PROCEDURES, Options::default()),
            ColumnFamilyDescriptor::new(cf::MEMORY_EVENT_INDEX, Options::default()),
            ColumnFamilyDescriptor::new(cf::MEMORY_CONFIG, Options::default()),
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
            continuity: crate::types::MemoryContinuity::default(),
        }
    }

    fn make_event(agent_id: AgentId, summary: &str, ts: DateTime<Utc>) -> AgentEvent {
        AgentEvent {
            event_id: AgentEventId::generate(),
            agent_id,
            event_type: "run".to_string(),
            summary: summary.to_string(),
            metadata: serde_json::Value::Null,
            importance: 0.6,
            access_count: 0,
            last_accessed: ts,
            timestamp: ts,
            continuity: crate::types::MemoryContinuity::default(),
        }
    }

    fn make_procedure(agent_id: AgentId, name: &str) -> Procedure {
        let now = Utc::now();
        Procedure {
            procedure_id: ProcedureId::generate(),
            agent_id,
            name: name.to_string(),
            trigger: "test trigger".to_string(),
            steps: vec!["build".to_string(), "push".to_string()],
            context_constraints: serde_json::Value::Null,
            success_rate: 0.8,
            execution_count: 5,
            last_used: now,
            created_at: now,
            updated_at: now,
            skill_name: None,
            skill_relevance: None,
            continuity: crate::types::MemoryContinuity::default(),
        }
    }

    fn sealed_store(dir: &std::path::Path) -> MemoryStore {
        let cipher = Arc::new(SealCipher::new(&[9u8; 32]));
        MemoryStore::with_cipher(test_db(dir), Some(cipher))
    }

    // ----------------------------------------------------------------
    // Sealed state-at-rest (Swarm TEE upgrade phase 5)
    // ----------------------------------------------------------------

    #[test]
    fn sealed_fact_roundtrip_and_ciphertext_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = sealed_store(dir.path());
        let agent = AgentId::generate();
        let fact = make_fact(agent, "secret-key", "secret-value");

        store.put_fact(&fact).unwrap();
        assert_eq!(
            store.get_fact(agent, fact.fact_id).unwrap().key,
            "secret-key"
        );
        assert_eq!(store.list_facts(agent).unwrap().len(), 1);
        assert!(store
            .get_fact_by_key(agent, "secret-key")
            .unwrap()
            .is_some());

        // Raw on-disk bytes must be a sealed envelope, not JSON.
        let cf = store.db().cf_handle(cf::MEMORY_FACTS).unwrap();
        let raw = store
            .db()
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert!(SealCipher::is_sealed(&raw));
        assert!(!raw
            .windows(b"secret-value".len())
            .any(|w| w == b"secret-value"));
    }

    #[test]
    fn sealed_event_and_procedure_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = sealed_store(dir.path());
        let agent = AgentId::generate();
        let now = Utc::now();

        store
            .put_event(&make_event(agent, "did a thing", now))
            .unwrap();
        assert_eq!(store.list_events(agent, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .list_events_since(agent, now - chrono::Duration::hours(1))
                .unwrap()
                .len(),
            1
        );

        let proc = make_procedure(agent, "deploy");
        store.put_procedure(&proc).unwrap();
        assert_eq!(
            store.get_procedure(agent, proc.procedure_id).unwrap().name,
            "deploy"
        );
        assert_eq!(store.list_procedures(agent).unwrap().len(), 1);
    }

    /// Plaintext mode stays byte-for-byte the legacy JSON format.
    #[test]
    fn plaintext_fact_on_disk_format_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(test_db(dir.path()));
        let agent = AgentId::generate();
        let fact = make_fact(agent, "k", "v");

        store.put_fact(&fact).unwrap();

        let cf = store.db().cf_handle(cf::MEMORY_FACTS).unwrap();
        let raw = store
            .db()
            .iterator_cf(&cf, IteratorMode::Start)
            .next()
            .unwrap()
            .unwrap()
            .1;
        assert_eq!(raw.to_vec(), serde_json::to_vec(&fact).unwrap());
        assert!(!SealCipher::is_sealed(&raw));
    }

    #[test]
    fn continuity_config_is_agent_scoped_persisted_and_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let store = sealed_store(dir.path());
        let configured_agent = AgentId::generate();
        let untouched_agent = AgentId::generate();
        let config = AgentContinuityConfig {
            use_memory: false,
            generate_memory: true,
            write_policy: crate::types::MemoryWritePolicy::Approval,
            retrieval_mode: crate::types::MemoryRetrievalMode::QueryAware,
            allow_user_scope: false,
            allow_workspace_scope: false,
        };

        store
            .put_continuity_config(configured_agent, &config)
            .unwrap();

        assert_eq!(
            store.get_continuity_config(configured_agent).unwrap(),
            config
        );
        assert_eq!(
            store.get_continuity_config(untouched_agent).unwrap(),
            AgentContinuityConfig::default()
        );

        let cf = store.db().cf_handle(cf::MEMORY_CONFIG).unwrap();
        let raw = store
            .db()
            .get_cf(&cf, configured_agent.as_bytes())
            .unwrap()
            .unwrap();
        assert!(SealCipher::is_sealed(&raw));
        assert!(!raw.windows(b"approval".len()).any(|w| w == b"approval"));
    }
}
