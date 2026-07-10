//! HTTP handlers for the memory CRUD surface.
//!
//! Wire types (`CreateFactBody`, `CreateEventBody`, …) live in
//! [`super::wire`]; the handlers here convert those into the
//! `aura_context_memory` domain types and dispatch to
//! [`aura_context_memory::MemoryStoreApi`].

use aura_context_memory::{AgentEvent, Fact, FactSource, MemoryStatus, MemoryStoreApi, Procedure};
use aura_core_types::{AgentEventId, FactId, ProcedureId};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;

use crate::gateway::handlers::util::parse_agent_id;
use crate::gateway::RouterState;

use super::wire::{
    BulkDeleteEventsBody, CreateEventBody, CreateFactBody, CreateProcedureBody,
    ProcedureListParams, UpdateFactBody, UpdateProcedureBody,
};

type ApiResult<T> = Result<Json<T>, (StatusCode, Json<serde_json::Value>)>;

fn parse_fact_id(hex: &str) -> Result<FactId, (StatusCode, Json<serde_json::Value>)> {
    FactId::from_hex(hex).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid fact_id: {e}") })),
        )
    })
}

fn parse_event_id(hex: &str) -> Result<AgentEventId, (StatusCode, Json<serde_json::Value>)> {
    AgentEventId::from_hex(hex).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid event_id: {e}") })),
        )
    })
}

fn parse_procedure_id(hex: &str) -> Result<ProcedureId, (StatusCode, Json<serde_json::Value>)> {
    ProcedureId::from_hex(hex).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid procedure_id: {e}") })),
        )
    })
}

fn memory_store(
    state: &RouterState,
) -> Result<&std::sync::Arc<dyn MemoryStoreApi>, (StatusCode, Json<serde_json::Value>)> {
    state
        .memory_manager
        .as_ref()
        .map(|mm| mm.store())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "memory system not configured" })),
            )
        })
}

fn store_err(e: aura_context_memory::MemoryError) -> (StatusCode, Json<serde_json::Value>) {
    let msg = e.to_string();
    let status = if msg.contains("not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(serde_json::json!({ "error": msg })))
}

// ============================================================================
// Facts
// ============================================================================

pub(in crate::gateway) async fn list_facts(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<Vec<Fact>> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    store.list_facts(agent_id).map(Json).map_err(store_err)
}

pub(in crate::gateway) async fn get_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    store
        .get_fact(agent_id, fact_id)
        .map(Json)
        .map_err(store_err)
}

pub(in crate::gateway) async fn get_fact_by_key(
    State(state): State<RouterState>,
    Path((agent_hex, key)): Path<(String, String)>,
) -> ApiResult<serde_json::Value> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    match store.get_fact_by_key(agent_id, &key) {
        Ok(Some(fact)) => {
            // Phase 5 (error-handling polish): the previous
            // `.unwrap_or_default()` quietly returned an empty
            // `serde_json::Value::Null` to the caller if `Fact`
            // serialization ever failed (e.g. a non-string map key
            // sneaks into a future schema). Surface the error as a
            // 500 so the misbehaviour is visible instead of looking
            // like a successful empty response.
            let value = serde_json::to_value(&fact).map_err(|e| {
                tracing::error!(
                    error = %e,
                    agent_id = %agent_hex,
                    key,
                    "memory router: serialising Fact for get_fact_by_key failed"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": format!("failed to serialize fact: {e}"),
                    })),
                )
            })?;
            Ok(Json(value))
        }
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "fact not found for key" })),
        )),
        Err(e) => Err(store_err(e)),
    }
}

pub(in crate::gateway) async fn create_fact(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(body): Json<CreateFactBody>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let source = match body.source.as_deref() {
        Some("user_provided") => FactSource::UserProvided,
        Some("consolidated") => FactSource::Consolidated,
        _ => FactSource::Extracted,
    };
    let fact = Fact {
        fact_id: FactId::generate(),
        agent_id,
        key: body.key,
        value: body.value,
        confidence: body.confidence,
        source,
        importance: body.importance,
        access_count: 0,
        last_accessed: now,
        created_at: now,
        updated_at: now,
        continuity: body.continuity.unwrap_or_default(),
    };
    store.put_fact(&fact).map_err(store_err)?;
    Ok(Json(fact))
}

pub(in crate::gateway) async fn update_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
    Json(body): Json<UpdateFactBody>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    let mut fact = store.get_fact(agent_id, fact_id).map_err(store_err)?;
    if let Some(key) = body.key {
        fact.key = key;
    }
    if let Some(value) = body.value {
        fact.value = value;
    }
    if let Some(confidence) = body.confidence {
        fact.confidence = confidence;
    }
    if let Some(importance) = body.importance {
        fact.importance = importance;
    }
    fact.updated_at = Utc::now();
    if let Some(continuity) = body.continuity {
        if fact.continuity.status == MemoryStatus::Pending
            && continuity.status == MemoryStatus::Active
        {
            for mut previous in store.list_facts(agent_id).map_err(store_err)? {
                if previous.fact_id != fact.fact_id
                    && previous.key == fact.key
                    && previous.continuity.status == MemoryStatus::Active
                {
                    previous.continuity.status = MemoryStatus::Superseded;
                    previous.continuity.superseded_by = Some(fact.fact_id.to_hex());
                    previous.updated_at = Utc::now();
                    store.put_fact(&previous).map_err(store_err)?;
                }
            }
        }
        fact.continuity = continuity;
    }
    if let Some(ref s) = body.source {
        fact.source = match s.as_str() {
            "user_provided" => FactSource::UserProvided,
            "consolidated" => FactSource::Consolidated,
            _ => FactSource::Extracted,
        };
    }
    store.put_fact(&fact).map_err(store_err)?;
    Ok(Json(fact))
}

pub(in crate::gateway) async fn delete_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    store.delete_fact(agent_id, fact_id).map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================================
// Events
// ============================================================================

pub(in crate::gateway) async fn list_events(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<Vec<AgentEvent>> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    store
        .list_events(agent_id, 1000)
        .map(Json)
        .map_err(store_err)
}

pub(in crate::gateway) async fn create_event(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(body): Json<CreateEventBody>,
) -> ApiResult<AgentEvent> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let event = AgentEvent {
        event_id: AgentEventId::generate(),
        agent_id,
        event_type: body.event_type,
        summary: body.summary,
        metadata: body.metadata,
        importance: body.importance,
        access_count: 0,
        last_accessed: now,
        timestamp: now,
        continuity: body.continuity.unwrap_or_default(),
    };
    store.put_event(&event).map_err(store_err)?;
    Ok(Json(event))
}

pub(in crate::gateway) async fn delete_event(
    State(state): State<RouterState>,
    Path((agent_hex, event_hex)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let event_id = parse_event_id(&event_hex)?;
    store.delete_event(agent_id, event_id).map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn bulk_delete_events(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(body): Json<BulkDeleteEventsBody>,
) -> ApiResult<serde_json::Value> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let deleted = store
        .delete_events_before(agent_id, body.before)
        .map_err(store_err)?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

// ============================================================================
// Procedures
// ============================================================================

pub(in crate::gateway) async fn list_procedures(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<ProcedureListParams>,
) -> ApiResult<Vec<Procedure>> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut procs = store.list_procedures(agent_id).map_err(store_err)?;

    if let Some(ref skill) = params.skill {
        procs.retain(|p| p.skill_name.as_deref() == Some(skill.as_str()));
    }
    if let Some(min_rel) = params.min_relevance {
        procs.retain(|p| p.skill_relevance.unwrap_or(0.0) >= min_rel);
    }

    Ok(Json(procs))
}

pub(in crate::gateway) async fn get_procedure(
    State(state): State<RouterState>,
    Path((agent_hex, proc_hex)): Path<(String, String)>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    store
        .get_procedure(agent_id, proc_id)
        .map(Json)
        .map_err(store_err)
}

pub(in crate::gateway) async fn create_procedure(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(body): Json<CreateProcedureBody>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let proc = Procedure {
        procedure_id: ProcedureId::generate(),
        agent_id,
        name: body.name,
        trigger: body.trigger,
        steps: body.steps,
        context_constraints: body.context_constraints,
        success_rate: 0.0,
        execution_count: 0,
        last_used: now,
        created_at: now,
        updated_at: now,
        skill_name: body.skill_name,
        skill_relevance: body.skill_relevance,
        continuity: body.continuity.unwrap_or_default(),
    };
    store.put_procedure(&proc).map_err(store_err)?;
    Ok(Json(proc))
}

pub(in crate::gateway) async fn update_procedure(
    State(state): State<RouterState>,
    Path((agent_hex, proc_hex)): Path<(String, String)>,
    Json(body): Json<UpdateProcedureBody>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    let mut proc = store.get_procedure(agent_id, proc_id).map_err(store_err)?;
    if let Some(name) = body.name {
        proc.name = name;
    }
    if let Some(trigger) = body.trigger {
        proc.trigger = trigger;
    }
    if let Some(steps) = body.steps {
        proc.steps = steps;
    }
    if let Some(context_constraints) = body.context_constraints {
        proc.context_constraints = context_constraints;
    }
    if body.skill_name.is_some() || body.skill_relevance.is_some() {
        proc.skill_name = body.skill_name;
        proc.skill_relevance = body.skill_relevance;
    }
    if let Some(success_rate) = body.success_rate {
        proc.success_rate = success_rate;
    }
    proc.updated_at = Utc::now();
    if let Some(continuity) = body.continuity {
        proc.continuity = continuity;
    }
    store.put_procedure(&proc).map_err(store_err)?;
    Ok(Json(proc))
}

pub(in crate::gateway) async fn delete_procedure(
    State(state): State<RouterState>,
    Path((agent_hex, proc_hex)): Path<(String, String)>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    store
        .delete_procedure(agent_id, proc_id)
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================================
// Aggregates
// ============================================================================

pub(in crate::gateway) async fn snapshot(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<aura_context_memory::MemoryPacket> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let facts = store.list_facts(agent_id).map_err(store_err)?;
    let events = store.list_events(agent_id, 1000).map_err(store_err)?;
    let procedures = store.list_procedures(agent_id).map_err(store_err)?;
    Ok(Json(aura_context_memory::MemoryPacket {
        facts,
        events,
        procedures,
        trace: None,
    }))
}

pub(in crate::gateway) async fn wipe(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    store.delete_all(agent_id).map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn stats(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<aura_context_memory::MemoryStats> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    store.stats(agent_id).map(Json).map_err(store_err)
}

// ============================================================================
// Agent Continuity controls and evidence
// ============================================================================

pub(in crate::gateway) async fn get_continuity_config(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<aura_context_memory::AgentContinuityConfig> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let manager = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    manager
        .continuity_config(agent_id)
        .await
        .map(Json)
        .map_err(store_err)
}

pub(in crate::gateway) async fn update_continuity_config(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Json(config): Json<aura_context_memory::AgentContinuityConfig>,
) -> ApiResult<aura_context_memory::AgentContinuityConfig> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let manager = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    manager
        .save_continuity_config(agent_id, config)
        .await
        .map(Json)
        .map_err(store_err)
}

pub(in crate::gateway) async fn latest_retrieval_trace(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<serde_json::Value> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let manager = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    Ok(Json(match manager.latest_retrieval_trace(agent_id) {
        Some(trace) => serde_json::to_value(trace).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
        })?,
        None => serde_json::Value::Null,
    }))
}

// ============================================================================
// Consolidation
// ============================================================================

pub(in crate::gateway) async fn consolidate(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
) -> ApiResult<aura_context_memory::ConsolidationReport> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let mm = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    mm.consolidate(agent_id).await.map(Json).map_err(store_err)
}
