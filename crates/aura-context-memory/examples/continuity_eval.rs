//! Deterministic Agent Continuity benchmark and cross-agent report adapter.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aura_context_memory::{
    Fact, FactSource, MemoryContinuity, MemoryProvenance, MemoryQueryContext, MemoryRetrievalMode,
    MemoryRetriever, MemoryScope, MemorySensitivity, MemoryStatus, MemoryStore, MemoryStoreApi,
    Procedure, RetrievalConfig,
};
use aura_core_types::{AgentId, FactId, ProcedureId};
use chrono::Utc;
use rocksdb::{ColumnFamilyDescriptor, DBWithThreadMode, MultiThreaded, Options};
use serde::{Deserialize, Serialize};

const DEFAULT_SUITE: &str = include_str!("../../../evals/memory_continuity/scenarios.json");

#[derive(Debug, Deserialize)]
struct EvalSuite {
    version: u32,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    id: String,
    query: String,
    active_skills: Vec<String>,
    allow_user_scope: bool,
    allow_workspace_scope: bool,
    expected_ids: Vec<String>,
    records: Vec<EvalRecord>,
}

#[derive(Debug, Deserialize)]
struct EvalRecord {
    kind: String,
    id: String,
    key: String,
    value: String,
    #[serde(default)]
    trigger: String,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    skill_name: Option<String>,
    importance: f32,
    confidence: f32,
    status: MemoryStatus,
    sensitivity: MemorySensitivity,
    scope: MemoryScope,
    pinned: bool,
}

#[derive(Debug, Deserialize)]
struct ExternalComparison {
    agent: String,
    runs: Vec<ExternalRun>,
}

#[derive(Debug, Deserialize)]
struct ExternalRun {
    scenario_id: String,
    recalled_ids: Vec<String>,
    #[serde(default)]
    estimated_tokens: usize,
    #[serde(default)]
    duration_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ScenarioResult {
    scenario_id: String,
    recalled_ids: Vec<String>,
    expected_ids: Vec<String>,
    relevant_selected: usize,
    forbidden_selected: Vec<String>,
    estimated_tokens: usize,
    duration_ms: u64,
}

#[derive(Debug, Serialize)]
struct AgentReport {
    agent: String,
    scenario_count: usize,
    recall_at_k: f64,
    precision_at_k: f64,
    mean_reciprocal_rank: f64,
    forbidden_recall_rate: f64,
    average_estimated_tokens: f64,
    p95_duration_ms: u64,
    runs: Vec<ScenarioResult>,
}

#[derive(Debug, Serialize)]
struct Thresholds {
    minimum_recall_at_k: f64,
    minimum_precision_at_k: f64,
    maximum_forbidden_recall_rate: f64,
    maximum_average_estimated_tokens: f64,
}

#[derive(Debug, Serialize)]
struct SuiteReport {
    suite_version: u32,
    generated_at: String,
    thresholds: Thresholds,
    aura_passed: bool,
    aura_improvement_vs_salience: AuraImprovement,
    reports: Vec<AgentReport>,
}

#[derive(Debug, Serialize)]
struct AuraImprovement {
    recall_at_k_delta: f64,
    precision_at_k_delta: f64,
    average_estimated_tokens_reduction: f64,
    estimated_token_reduction_percent: f64,
}

fn test_db(dir: &Path) -> Arc<DBWithThreadMode<MultiThreaded>> {
    let mut options = Options::default();
    options.create_if_missing(true);
    options.create_missing_column_families(true);
    let cfs = [
        "memory_facts",
        "memory_events",
        "memory_procedures",
        "memory_event_index",
        "memory_config",
    ]
    .into_iter()
    .map(|name| ColumnFamilyDescriptor::new(name, Options::default()));
    Arc::new(
        DBWithThreadMode::<MultiThreaded>::open_cf_descriptors(&options, dir, cfs)
            .expect("open benchmark store"),
    )
}

fn continuity(record: &EvalRecord) -> MemoryContinuity {
    MemoryContinuity {
        scope: record.scope,
        status: record.status,
        sensitivity: record.sensitivity,
        pinned: record.pinned,
        provenance: MemoryProvenance {
            excerpt: Some(format!("benchmark record {}", record.id)),
            extractor_model: Some("benchmark-fixture".to_string()),
            ..MemoryProvenance::default()
        },
        superseded_by: None,
    }
}

fn forbidden_ids(scenario: &Scenario) -> HashSet<String> {
    scenario
        .records
        .iter()
        .filter(|record| {
            record.status != MemoryStatus::Active
                || record.sensitivity == MemorySensitivity::Sensitive
                || (record.scope == MemoryScope::User && !scenario.allow_user_scope)
                || (record.scope == MemoryScope::Workspace && !scenario.allow_workspace_scope)
        })
        .map(|record| record.id.clone())
        .collect()
}

async fn run_aura(suite: &EvalSuite, mode: MemoryRetrievalMode, agent_name: &str) -> AgentReport {
    let temp = tempfile::tempdir().expect("create benchmark directory");
    let store: Arc<dyn MemoryStoreApi> = Arc::new(MemoryStore::new(test_db(temp.path())));
    let retriever = MemoryRetriever::new(Arc::clone(&store), RetrievalConfig::default());
    let mut runs = Vec::with_capacity(suite.scenarios.len());

    for scenario in &suite.scenarios {
        let agent_id = AgentId::generate();
        let now = Utc::now();
        let mut memory_ids = HashMap::new();

        for record in &scenario.records {
            match record.kind.as_str() {
                "fact" => {
                    let fact_id = FactId::generate();
                    memory_ids.insert(fact_id.to_hex(), record.id.clone());
                    store
                        .put_fact(&Fact {
                            fact_id,
                            agent_id,
                            key: record.key.clone(),
                            value: serde_json::Value::String(record.value.clone()),
                            confidence: record.confidence,
                            source: FactSource::Extracted,
                            importance: record.importance,
                            access_count: 0,
                            last_accessed: now,
                            created_at: now,
                            updated_at: now,
                            continuity: continuity(record),
                        })
                        .expect("seed benchmark fact");
                }
                "procedure" => {
                    let procedure_id = ProcedureId::generate();
                    memory_ids.insert(procedure_id.to_hex(), record.id.clone());
                    store
                        .put_procedure(&Procedure {
                            procedure_id,
                            agent_id,
                            name: record.key.clone(),
                            trigger: record.trigger.clone(),
                            steps: record.steps.clone(),
                            context_constraints: serde_json::Value::Null,
                            success_rate: record.confidence,
                            execution_count: 1,
                            last_used: now,
                            created_at: now,
                            updated_at: now,
                            skill_name: record.skill_name.clone(),
                            skill_relevance: record.skill_name.as_ref().map(|_| 0.8),
                            continuity: continuity(record),
                        })
                        .expect("seed benchmark procedure");
                }
                other => panic!("unsupported benchmark record kind: {other}"),
            }
        }

        let packet = retriever
            .retrieve_with_query(
                agent_id,
                MemoryQueryContext {
                    text: scenario.query.clone(),
                    active_skills: scenario.active_skills.clone(),
                    allow_user_scope: scenario.allow_user_scope,
                    allow_workspace_scope: scenario.allow_workspace_scope,
                },
                mode,
            )
            .await
            .expect("run Aura retrieval");
        let trace = packet.trace.expect("retrieval trace");
        let recalled_ids: Vec<String> = trace
            .selections
            .iter()
            .filter_map(|selection| memory_ids.get(&selection.memory_id).cloned())
            .collect();
        runs.push(score_scenario(
            scenario,
            recalled_ids,
            trace.estimated_tokens,
            trace.duration_ms,
        ));
    }

    aggregate(agent_name.to_string(), runs)
}

fn score_scenario(
    scenario: &Scenario,
    recalled_ids: Vec<String>,
    estimated_tokens: usize,
    duration_ms: u64,
) -> ScenarioResult {
    let expected: HashSet<&str> = scenario.expected_ids.iter().map(String::as_str).collect();
    let forbidden = forbidden_ids(scenario);
    ScenarioResult {
        scenario_id: scenario.id.clone(),
        relevant_selected: recalled_ids
            .iter()
            .filter(|id| expected.contains(id.as_str()))
            .count(),
        forbidden_selected: recalled_ids
            .iter()
            .filter(|id| forbidden.contains(id.as_str()))
            .cloned()
            .collect(),
        recalled_ids,
        expected_ids: scenario.expected_ids.clone(),
        estimated_tokens,
        duration_ms,
    }
}

fn aggregate(agent: String, runs: Vec<ScenarioResult>) -> AgentReport {
    let scenario_count = runs.len();
    let expected_count: usize = runs.iter().map(|run| run.expected_ids.len()).sum();
    let relevant_count: usize = runs.iter().map(|run| run.relevant_selected).sum();
    let selected_count: usize = runs.iter().map(|run| run.recalled_ids.len()).sum();
    let forbidden_count: usize = runs.iter().map(|run| run.forbidden_selected.len()).sum();
    let reciprocal_rank_sum: f64 = runs
        .iter()
        .map(|run| {
            run.recalled_ids
                .iter()
                .position(|id| run.expected_ids.contains(id))
                .map_or(0.0, |rank| 1.0 / (rank + 1) as f64)
        })
        .sum();
    let mut durations: Vec<u64> = runs.iter().map(|run| run.duration_ms).collect();
    durations.sort_unstable();
    let p95_index = durations.len().saturating_sub(1) * 95 / 100;

    AgentReport {
        agent,
        scenario_count,
        recall_at_k: relevant_count as f64 / expected_count.max(1) as f64,
        precision_at_k: relevant_count as f64 / selected_count.max(1) as f64,
        mean_reciprocal_rank: reciprocal_rank_sum / scenario_count.max(1) as f64,
        forbidden_recall_rate: forbidden_count as f64 / selected_count.max(1) as f64,
        average_estimated_tokens: runs.iter().map(|run| run.estimated_tokens).sum::<usize>() as f64
            / scenario_count.max(1) as f64,
        p95_duration_ms: durations.get(p95_index).copied().unwrap_or_default(),
        runs,
    }
}

fn score_external(suite: &EvalSuite, comparison: ExternalComparison) -> AgentReport {
    let by_scenario: HashMap<String, ExternalRun> = comparison
        .runs
        .into_iter()
        .map(|run| (run.scenario_id.clone(), run))
        .collect();
    let runs = suite
        .scenarios
        .iter()
        .map(|scenario| {
            let run = by_scenario.get(&scenario.id);
            score_scenario(
                scenario,
                run.map(|item| item.recalled_ids.clone())
                    .unwrap_or_default(),
                run.map_or(0, |item| item.estimated_tokens),
                run.map_or(0, |item| item.duration_ms),
            )
        })
        .collect();
    aggregate(comparison.agent, runs)
}

fn arguments() -> (Option<PathBuf>, Vec<PathBuf>) {
    let mut output = None;
    let mut comparisons = Vec::new();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--output" => {
                index += 1;
                output = args.get(index).map(PathBuf::from);
            }
            "--comparison" => {
                index += 1;
                if let Some(path) = args.get(index) {
                    comparisons.push(PathBuf::from(path));
                }
            }
            unknown => panic!("unknown argument: {unknown}"),
        }
        index += 1;
    }
    (output, comparisons)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let (output, comparison_paths) = arguments();
    let suite: EvalSuite = serde_json::from_str(DEFAULT_SUITE).expect("parse benchmark suite");
    let aura = run_aura(
        &suite,
        MemoryRetrievalMode::QueryAware,
        "aura-query-aware-v1",
    )
    .await;
    let salience_baseline = run_aura(
        &suite,
        MemoryRetrievalMode::Salience,
        "aura-salience-baseline",
    )
    .await;
    let thresholds = Thresholds {
        minimum_recall_at_k: 0.9,
        minimum_precision_at_k: 0.8,
        maximum_forbidden_recall_rate: 0.0,
        maximum_average_estimated_tokens: 800.0,
    };
    let aura_passed = aura.recall_at_k >= thresholds.minimum_recall_at_k
        && aura.precision_at_k >= thresholds.minimum_precision_at_k
        && aura.forbidden_recall_rate <= thresholds.maximum_forbidden_recall_rate
        && aura.average_estimated_tokens <= thresholds.maximum_average_estimated_tokens;
    let aura_improvement_vs_salience = AuraImprovement {
        recall_at_k_delta: aura.recall_at_k - salience_baseline.recall_at_k,
        precision_at_k_delta: aura.precision_at_k - salience_baseline.precision_at_k,
        average_estimated_tokens_reduction: salience_baseline.average_estimated_tokens
            - aura.average_estimated_tokens,
        estimated_token_reduction_percent: if salience_baseline.average_estimated_tokens > 0.0 {
            (salience_baseline.average_estimated_tokens - aura.average_estimated_tokens)
                / salience_baseline.average_estimated_tokens
                * 100.0
        } else {
            0.0
        },
    };
    let mut reports = vec![aura, salience_baseline];

    for path in comparison_paths {
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read comparison {}: {error}", path.display()));
        let comparison: ExternalComparison = serde_json::from_str(&raw)
            .unwrap_or_else(|error| panic!("parse comparison {}: {error}", path.display()));
        reports.push(score_external(&suite, comparison));
    }

    let report = SuiteReport {
        suite_version: suite.version,
        generated_at: Utc::now().to_rfc3339(),
        thresholds,
        aura_passed,
        aura_improvement_vs_salience,
        reports,
    };
    let json = serde_json::to_string_pretty(&report).expect("serialize benchmark report");
    if let Some(path) = output {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create report directory");
        }
        std::fs::write(&path, format!("{json}\n")).expect("write benchmark report");
    }
    println!("{json}");

    if !aura_passed {
        std::process::exit(2);
    }
}
