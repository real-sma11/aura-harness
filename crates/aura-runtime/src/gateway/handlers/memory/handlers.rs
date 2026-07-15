//! HTTP handlers for the memory CRUD surface.
//!
//! Wire types (`CreateFactBody`, `CreateEventBody`, …) live in
//! [`super::wire`]; the handlers here convert those into the
//! `aura_context_memory` domain types and dispatch to
//! [`aura_context_memory::MemoryStoreApi`].

use aura_context_memory::{
    AgentEvent, Fact, FactSource, MemoryAccessContext, MemoryScope, MemoryStatus, MemoryStoreApi,
    Procedure,
};
use aura_core_types::{AgentEventId, AgentId, FactId, ProcedureId};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;

use crate::gateway::handlers::util::parse_agent_id;
use crate::gateway::RouterState;

use super::wire::{
    BulkDeleteEventsBody, CreateEventBody, CreateFactBody, CreateProcedureBody, MemoryAccessParams,
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

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message.into() })),
    )
}

fn scoped_partitions(agent_id: AgentId, params: &MemoryAccessParams) -> Vec<AgentId> {
    let access = params.access();
    if let Some(scope) = params.scope {
        match scope {
            MemoryScope::Agent if access.project_id.is_none() => vec![agent_id],
            MemoryScope::Project if access.project_id.is_none() => Vec::new(),
            MemoryScope::User if access.user_id.is_none() => Vec::new(),
            _ => vec![access.storage_id(agent_id, scope)],
        }
    } else {
        access.readable_partitions(agent_id, true, true)
    }
}

fn target_partition(
    agent_id: AgentId,
    access: &MemoryAccessContext,
    scope: MemoryScope,
) -> Result<AgentId, (StatusCode, Json<serde_json::Value>)> {
    match scope {
        MemoryScope::Project if access.project_id.is_none() => Err(bad_request(
            "project_id is required for project-scoped memory",
        )),
        MemoryScope::User if access.user_id.is_none() => {
            Err(bad_request("user_id is required for personal memory"))
        }
        _ => Ok(
            if access.project_id.is_none() && scope == MemoryScope::Agent {
                // Preserve the v1 API contract for project-less callers.
                agent_id
            } else {
                access.storage_id(agent_id, scope)
            },
        ),
    }
}

fn find_fact(
    store: &dyn MemoryStoreApi,
    partitions: &[AgentId],
    fact_id: FactId,
) -> Result<Fact, (StatusCode, Json<serde_json::Value>)> {
    for partition in partitions {
        match store.get_fact(*partition, fact_id) {
            Ok(fact) => return Ok(fact),
            Err(error) if error.to_string().contains("not found") => {}
            Err(error) => return Err(store_err(error)),
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "fact not found" })),
    ))
}

fn find_procedure(
    store: &dyn MemoryStoreApi,
    partitions: &[AgentId],
    procedure_id: ProcedureId,
) -> Result<Procedure, (StatusCode, Json<serde_json::Value>)> {
    for partition in partitions {
        match store.get_procedure(*partition, procedure_id) {
            Ok(procedure) => return Ok(procedure),
            Err(error) if error.to_string().contains("not found") => {}
            Err(error) => return Err(store_err(error)),
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "procedure not found" })),
    ))
}

fn find_event(
    store: &dyn MemoryStoreApi,
    partitions: &[AgentId],
    event_id: AgentEventId,
) -> Result<AgentEvent, (StatusCode, Json<serde_json::Value>)> {
    for partition in partitions {
        let events = store.list_events(*partition, 10_000).map_err(store_err)?;
        if let Some(event) = events.into_iter().find(|event| event.event_id == event_id) {
            return Ok(event);
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "event not found" })),
    ))
}

// ============================================================================
// Facts
// ============================================================================

pub(in crate::gateway) async fn list_facts(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<Vec<Fact>> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut facts = Vec::new();
    for partition in scoped_partitions(agent_id, &params) {
        facts.extend(store.list_facts(partition).map_err(store_err)?);
    }
    facts.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(Json(facts))
}

pub(in crate::gateway) async fn get_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    find_fact(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        fact_id,
    )
    .map(Json)
}

pub(in crate::gateway) async fn get_fact_by_key(
    State(state): State<RouterState>,
    Path((agent_hex, key)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<serde_json::Value> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    for partition in scoped_partitions(agent_id, &params) {
        match store.get_fact_by_key(partition, &key) {
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
                return Ok(Json(value));
            }
            Ok(None) => {}
            Err(e) => return Err(store_err(e)),
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "fact not found for key" })),
    ))
}

pub(in crate::gateway) async fn create_fact(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<CreateFactBody>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let access = params.access();
    let mut continuity = body.continuity.unwrap_or_default();
    continuity.provenance.contributor_agent_id = Some(agent_id.to_hex());
    let agent_id = target_partition(agent_id, &access, continuity.scope)?;
    continuity.provenance.project_id = access.project_id.clone();
    continuity.provenance.user_id = access.user_id.clone();
    let source = match body.source.as_deref() {
        Some("user_provided") => FactSource::UserProvided,
        Some("consolidated") => FactSource::Consolidated,
        _ => FactSource::Extracted,
    };
    if matches!(continuity.scope, MemoryScope::Project | MemoryScope::User)
        && store
            .get_fact_by_key(agent_id, &body.key)
            .map_err(store_err)?
            .is_some_and(|existing| existing.value != body.value)
    {
        continuity.status = MemoryStatus::Pending;
    }
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
        continuity,
    };
    store.put_fact(&fact).map_err(store_err)?;
    Ok(Json(fact))
}

pub(in crate::gateway) async fn update_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<UpdateFactBody>,
) -> ApiResult<Fact> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    let partitions = scoped_partitions(agent_id, &params);
    let mut fact = find_fact(store.as_ref(), &partitions, fact_id)?;
    let source_partition = fact.agent_id;
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
            for mut previous in store.list_facts(source_partition).map_err(store_err)? {
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
    let access = params.access();
    let destination = target_partition(agent_id, &access, fact.continuity.scope)?;
    fact.continuity
        .provenance
        .contributor_agent_id
        .get_or_insert_with(|| agent_id.to_hex());
    fact.continuity.provenance.project_id = access.project_id.clone();
    fact.continuity.provenance.user_id = access.user_id.clone();
    if destination != source_partition
        && matches!(
            fact.continuity.scope,
            MemoryScope::Project | MemoryScope::User
        )
        && store
            .get_fact_by_key(destination, &fact.key)
            .map_err(store_err)?
            .is_some_and(|existing| {
                existing.fact_id != fact.fact_id && existing.value != fact.value
            })
    {
        fact.continuity.status = MemoryStatus::Pending;
    }
    fact.agent_id = destination;
    store.put_fact(&fact).map_err(store_err)?;
    if source_partition != destination {
        store
            .delete_fact(source_partition, fact.fact_id)
            .map_err(store_err)?;
    }
    Ok(Json(fact))
}

pub(in crate::gateway) async fn delete_fact(
    State(state): State<RouterState>,
    Path((agent_hex, fact_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let fact_id = parse_fact_id(&fact_hex)?;
    let fact = find_fact(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        fact_id,
    )?;
    store
        .delete_fact(fact.agent_id, fact_id)
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================================
// Events
// ============================================================================

pub(in crate::gateway) async fn list_events(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<Vec<AgentEvent>> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut events = Vec::new();
    for partition in scoped_partitions(agent_id, &params) {
        events.extend(store.list_events(partition, 1000).map_err(store_err)?);
    }
    events.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    events.truncate(1000);
    Ok(Json(events))
}

pub(in crate::gateway) async fn create_event(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<CreateEventBody>,
) -> ApiResult<AgentEvent> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let access = params.access();
    let mut continuity = body.continuity.unwrap_or_default();
    continuity.provenance.contributor_agent_id = Some(agent_id.to_hex());
    let agent_id = target_partition(agent_id, &access, continuity.scope)?;
    continuity.provenance.project_id = access.project_id.clone();
    continuity.provenance.user_id = access.user_id.clone();
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
        continuity,
    };
    store.put_event(&event).map_err(store_err)?;
    Ok(Json(event))
}

pub(in crate::gateway) async fn delete_event(
    State(state): State<RouterState>,
    Path((agent_hex, event_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let event_id = parse_event_id(&event_hex)?;
    let event = find_event(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        event_id,
    )?;
    store
        .delete_event(event.agent_id, event_id)
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn bulk_delete_events(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<BulkDeleteEventsBody>,
) -> ApiResult<serde_json::Value> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut deleted = 0;
    for partition in scoped_partitions(agent_id, &params) {
        deleted += store
            .delete_events_before(partition, body.before)
            .map_err(store_err)?;
    }
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
    let mut procs = Vec::new();
    for partition in scoped_partitions(agent_id, &params.access) {
        procs.extend(store.list_procedures(partition).map_err(store_err)?);
    }

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
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    find_procedure(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        proc_id,
    )
    .map(Json)
}

pub(in crate::gateway) async fn create_procedure(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<CreateProcedureBody>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let now = Utc::now();
    let access = params.access();
    let mut continuity = body.continuity.unwrap_or_default();
    continuity.provenance.contributor_agent_id = Some(agent_id.to_hex());
    let agent_id = target_partition(agent_id, &access, continuity.scope)?;
    continuity.provenance.project_id = access.project_id.clone();
    continuity.provenance.user_id = access.user_id.clone();
    if matches!(continuity.scope, MemoryScope::Project | MemoryScope::User)
        && store
            .list_procedures(agent_id)
            .map_err(store_err)?
            .into_iter()
            .any(|existing| {
                existing.name == body.name
                    && existing.continuity.status == MemoryStatus::Active
                    && (existing.trigger != body.trigger || existing.steps != body.steps)
            })
    {
        continuity.status = MemoryStatus::Pending;
    }
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
        continuity,
    };
    store.put_procedure(&proc).map_err(store_err)?;
    Ok(Json(proc))
}

pub(in crate::gateway) async fn update_procedure(
    State(state): State<RouterState>,
    Path((agent_hex, proc_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
    Json(body): Json<UpdateProcedureBody>,
) -> ApiResult<Procedure> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    let mut proc = find_procedure(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        proc_id,
    )?;
    let source_partition = proc.agent_id;
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
        if proc.continuity.status == MemoryStatus::Pending
            && continuity.status == MemoryStatus::Active
        {
            for mut previous in store.list_procedures(source_partition).map_err(store_err)? {
                if previous.procedure_id != proc.procedure_id
                    && previous.name == proc.name
                    && previous.continuity.status == MemoryStatus::Active
                {
                    previous.continuity.status = MemoryStatus::Superseded;
                    previous.continuity.superseded_by = Some(proc.procedure_id.to_hex());
                    previous.updated_at = Utc::now();
                    store.put_procedure(&previous).map_err(store_err)?;
                }
            }
        }
        proc.continuity = continuity;
    }
    let access = params.access();
    let destination = target_partition(agent_id, &access, proc.continuity.scope)?;
    proc.continuity
        .provenance
        .contributor_agent_id
        .get_or_insert_with(|| agent_id.to_hex());
    proc.continuity.provenance.project_id = access.project_id.clone();
    proc.continuity.provenance.user_id = access.user_id.clone();
    if destination != source_partition
        && matches!(
            proc.continuity.scope,
            MemoryScope::Project | MemoryScope::User
        )
        && store
            .list_procedures(destination)
            .map_err(store_err)?
            .into_iter()
            .any(|existing| {
                existing.procedure_id != proc.procedure_id
                    && existing.name == proc.name
                    && existing.continuity.status == MemoryStatus::Active
                    && (existing.trigger != proc.trigger || existing.steps != proc.steps)
            })
    {
        proc.continuity.status = MemoryStatus::Pending;
    }
    proc.agent_id = destination;
    store.put_procedure(&proc).map_err(store_err)?;
    if source_partition != destination {
        store
            .delete_procedure(source_partition, proc.procedure_id)
            .map_err(store_err)?;
    }
    Ok(Json(proc))
}

pub(in crate::gateway) async fn delete_procedure(
    State(state): State<RouterState>,
    Path((agent_hex, proc_hex)): Path<(String, String)>,
    Query(params): Query<MemoryAccessParams>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let proc_id = parse_procedure_id(&proc_hex)?;
    let procedure = find_procedure(
        store.as_ref(),
        &scoped_partitions(agent_id, &params),
        proc_id,
    )?;
    store
        .delete_procedure(procedure.agent_id, proc_id)
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ============================================================================
// Aggregates
// ============================================================================

pub(in crate::gateway) async fn snapshot(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<aura_context_memory::MemoryPacket> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut facts = Vec::new();
    let mut events = Vec::new();
    let mut procedures = Vec::new();
    for partition in scoped_partitions(agent_id, &params) {
        facts.extend(store.list_facts(partition).map_err(store_err)?);
        events.extend(store.list_events(partition, 1000).map_err(store_err)?);
        procedures.extend(store.list_procedures(partition).map_err(store_err)?);
    }
    facts.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    events.sort_by(|left, right| right.timestamp.cmp(&left.timestamp));
    procedures.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
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
    Query(params): Query<MemoryAccessParams>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let access = params.access();
    let partitions = if params.scope.is_some() {
        scoped_partitions(agent_id, &params)
    } else if access.project_id.is_some() {
        vec![access.storage_id(agent_id, MemoryScope::Agent)]
    } else {
        vec![agent_id]
    };
    for partition in partitions {
        store.delete_all(partition).map_err(store_err)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(in crate::gateway) async fn stats(
    State(state): State<RouterState>,
    Path(agent_hex): Path<String>,
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<aura_context_memory::MemoryStats> {
    let store = memory_store(&state)?;
    let agent_id = parse_agent_id(&agent_hex)?;
    let mut combined = aura_context_memory::MemoryStats {
        facts: 0,
        events: 0,
        procedures: 0,
    };
    for partition in scoped_partitions(agent_id, &params) {
        let stats = store.stats(partition).map_err(store_err)?;
        combined.facts += stats.facts;
        combined.events += stats.events;
        combined.procedures += stats.procedures;
    }
    Ok(Json(combined))
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
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<serde_json::Value> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let manager = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    let partition_id = params.access().storage_id(agent_id, MemoryScope::Agent);
    Ok(Json(match manager.latest_retrieval_trace(partition_id) {
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
    Query(params): Query<MemoryAccessParams>,
) -> ApiResult<aura_context_memory::ConsolidationReport> {
    let agent_id = parse_agent_id(&agent_hex)?;
    let mm = state.memory_manager.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "memory system not configured" })),
        )
    })?;
    let mut combined = aura_context_memory::ConsolidationReport::default();
    for partition in scoped_partitions(agent_id, &params) {
        let report = mm.consolidate(partition).await.map_err(store_err)?;
        combined.facts_merged += report.facts_merged;
        combined.facts_evolved += report.facts_evolved;
        combined.events_compressed += report.events_compressed;
        combined.events_deleted += report.events_deleted;
        combined.facts_forgotten += report.facts_forgotten;
        combined.procedures_forgotten += report.procedures_forgotten;
        combined.insights_created += report.insights_created;
    }
    Ok(Json(combined))
}
