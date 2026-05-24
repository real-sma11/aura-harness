//! Auto-decomposition of stuck dev-loop tasks.
//!
//! Phase 5 of the harness-v2 fix (plan `harness_task_completion_fix`).
//! When a dev-loop task fails with `ResearchLoopAbort` /
//! [`AutomatonError::NeedsDecomposition`] on its first retry (`attempt
//! == 1`), instead of repeating the same fresh-context run, the loop
//! invokes [`auto_decompose_task`] to split the parent into 2-5
//! self-contained subtasks via a single LLM call.
//!
//! The split is a best-effort signal: any failure path (insufficient
//! hints, model error, invalid JSON, cycle in dependencies, fewer than
//! 2 / more than [`MAX_SUBTASKS_PER_DECOMPOSITION`] subtasks)
//! propagates as a typed [`DecompositionError`], and the caller in
//! `tick.rs::record_task_failure` falls through to the original
//! `task_failed` emission so no failure event is ever lost.
//!
//! Pattern parallels Claude Code's `TodoWrite` + `Task` subagent split
//! when a single agent can't make progress on a multi-part request.
//! Pairs with `aura-os` Phase 6's
//! `RetryAction::RetryWithDecomposition` retry classification.

use std::collections::{HashMap, HashSet};

use aura_reasoner::{
    ContentBlock, Message, ModelProvider, ModelRequest, ModelRequestKind, ToolChoice,
};
use aura_tools::domain_tools::TaskDescriptor;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use aura_agent::prompts::{
    default_caps, extract_hints, resolve_hints, ResolveCaps, WorkspaceReader,
};

use super::validation::DecompositionHint;

/// Hard cap on the number of subtasks a single decomposition pass may
/// emit. The plan (`harness_task_completion_fix` Phase 5) calls for
/// 2-5 self-contained subtasks; responses outside that band are
/// refused so a single decomposition can never explode the per-task
/// budget into a fan-out of dozens of fresh agent runs.
pub const MAX_SUBTASKS_PER_DECOMPOSITION: usize = 5;

/// Minimum number of subtasks for a decomposition to be accepted.
/// One-subtask decompositions add no signal over a plain retry, so
/// we treat them as a failure to split and fall back to `task_failed`.
pub const MIN_SUBTASKS_PER_DECOMPOSITION: usize = 2;

/// Response budget for the decomposition LLM call. Small on purpose:
/// the splitter is a structured-output pass, not a planning agent.
const DECOMPOSITION_MAX_TOKENS: u32 = 2_000;

/// Per-path file-head budget when splicing the Phase-4 pre-resolved
/// context block into the splitter prompt. Smaller than the agent
/// loop's default to keep the splitter's request payload tight.
const DECOMPOSITION_RESOLVE_LINES_PER_PATH: usize = 20;

/// Soft cap on the rendered pre-resolved context block embedded in
/// the splitter prompt. The splitter needs hints about where the
/// agent got stuck — not a verbatim copy of the workspace.
const DECOMPOSITION_RESOLVE_BLOCK_CHARS: usize = 3_000;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Inputs required to run a single decomposition pass.
///
/// All references are borrowed for the duration of the call so callers
/// can keep their own ownership of the parent task, the hint produced
/// by [`super::validation::build_decomposition_hint`], and the
/// workspace reader created from the project folder. The
/// `ModelProvider` reference is the same one the dev-loop already
/// holds; we don't introduce a new provider, new API key plumbing, or
/// new dependencies beyond what `aura-automaton` already pulls in
/// (Phase 5 constraint).
pub struct DecompositionInput<'a> {
    pub parent_task: &'a TaskDescriptor,
    pub hint: &'a DecompositionHint,
    pub workspace: &'a dyn WorkspaceReader,
    pub provider: &'a dyn ModelProvider,
    /// Model to use for the splitter call. The dev-loop passes its
    /// configured `default_model` so the splitter inherits the same
    /// router/billing identifiers as the parent task run.
    pub model: String,
    pub auth_token: Option<String>,
    pub aura_org_id: Option<String>,
    pub aura_agent_id: Option<String>,
    pub aura_session_id: Option<String>,
    pub aura_project_id: Option<String>,
}

/// Successful decomposition: a vector of [`TaskDescriptor`]s with
/// deterministic ids, plus the model's one-paragraph reasoning so
/// callers can surface a "completed via decomposition" note in events.
#[derive(Debug, Clone)]
pub struct DecompositionResult {
    pub subtasks: Vec<TaskDescriptor>,
    pub reasoning: String,
}

/// Typed reasons a decomposition pass might fail. All variants are
/// treated identically by the caller — they're propagated up so the
/// gate in `tick.rs` falls back to the original `task_failed`
/// emission — but the variant labels are useful for logging and
/// future telemetry.
#[derive(Debug, Error)]
pub enum DecompositionError {
    /// The hint and resolved context don't carry enough information
    /// to formulate a meaningful split prompt. Returned BEFORE any
    /// model call so the loss is cheap. (Part of the documented
    /// public surface — currently no caller constructs this
    /// variant, but the helper may grow a pre-flight gate in a
    /// follow-up.)
    #[allow(dead_code)]
    #[error("not enough signal to split task: {0}")]
    InsufficientSignal(String),

    /// The provider's `complete` call failed.
    #[error("model error: {0}")]
    Model(String),

    /// Response JSON parsed cleanly but the shape didn't match the
    /// expected `{ subtasks: [..], reasoning: ".." }` schema.
    #[error("invalid decomposition shape: {0}")]
    InvalidShape(String),

    /// Fewer than [`MIN_SUBTASKS_PER_DECOMPOSITION`] subtasks. A
    /// single-subtask decomposition adds no signal over a plain
    /// retry.
    #[error("decomposition produced fewer than {} valid subtasks", MIN_SUBTASKS_PER_DECOMPOSITION)]
    TooFewSubtasks,

    /// More than [`MAX_SUBTASKS_PER_DECOMPOSITION`] subtasks. The
    /// cap exists to prevent a single decomposition from fanning out
    /// into a dozen fresh agent runs.
    #[error("decomposition produced more than {} subtasks", MAX_SUBTASKS_PER_DECOMPOSITION)]
    TooManySubtasks,

    /// Subtask dependency graph contains a cycle. The dev-loop's
    /// topological sort would loop forever, so we refuse the split.
    #[error("subtask dependency graph has a cycle")]
    Cyclic,
}

/// Run a single decomposition pass.
///
/// Best-effort: any failure (insufficient hint, model error, invalid
/// JSON, cycle, fewer than 2 / more than 5 subtasks) returns
/// [`DecompositionError`] so the caller can fall back to the original
/// `task_failed` emission. Never panics, never blocks indefinitely
/// (the IO inside `resolve_hints` is timeout-bounded, the LLM call
/// inherits the provider's existing timeout).
pub async fn auto_decompose_task(
    input: DecompositionInput<'_>,
) -> Result<DecompositionResult, DecompositionError> {
    let resolved_context_block = resolve_context_block(input.parent_task, input.workspace).await;

    let prompt = build_splitter_prompt(input.parent_task, input.hint, &resolved_context_block);

    let response = call_splitter(&input, prompt).await?;
    let raw_text = extract_text(&response.message)
        .ok_or_else(|| DecompositionError::InvalidShape("model returned no text content".into()))?;

    let parsed = parse_response(&raw_text)?;

    let subtasks = convert_to_descriptors(input.parent_task, &parsed)?;

    Ok(DecompositionResult {
        subtasks,
        reasoning: parsed.reasoning,
    })
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

/// Resolve the Phase-4 pre-resolved context block for the parent
/// task's description against `workspace`. Best-effort — any IO
/// failure / timeout / missing file silently drops the block, and
/// the splitter prompt simply omits it. The returned string is
/// empty when the description has no resolvable hints OR every
/// hint failed to resolve.
async fn resolve_context_block(
    parent: &TaskDescriptor,
    workspace: &dyn WorkspaceReader,
) -> String {
    let hints = extract_hints(&parent.description);
    if !hints.is_meaningful() {
        return String::new();
    }
    let caps = ResolveCaps {
        max_lines_per_path: DECOMPOSITION_RESOLVE_LINES_PER_PATH,
        max_block_chars: DECOMPOSITION_RESOLVE_BLOCK_CHARS,
        ..default_caps()
    };
    let resolved = resolve_hints(&hints, workspace, caps).await;
    if resolved.is_empty() {
        return String::new();
    }
    resolved.into_block()
}

/// Build the splitter prompt. Format is intentionally compact and
/// machine-grep-able: the model must return JSON ONLY with the
/// documented shape; commentary outside the JSON object is rejected
/// by [`parse_response`].
fn build_splitter_prompt(
    parent: &TaskDescriptor,
    hint: &DecompositionHint,
    resolved_context_block: &str,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "PARENT TASK: {}", parent.title);
    let _ = writeln!(out, "PARENT ID: {}", parent.id);
    out.push_str("\nDESCRIPTION:\n");
    out.push_str(&parent.description);
    if !parent.description.ends_with('\n') {
        out.push('\n');
    }

    if !resolved_context_block.is_empty() {
        out.push_str("\n");
        out.push_str(resolved_context_block);
        if !resolved_context_block.ends_with('\n') {
            out.push('\n');
        }
    }

    if !hint.failed_paths.is_empty() {
        out.push_str("\nFAILED PATHS (the previous attempt could not complete writes to):\n");
        for p in &hint.failed_paths {
            let _ = writeln!(out, "- {p}");
        }
    }
    if let Some(name) = &hint.last_pending_tool_name {
        let _ = writeln!(out, "\nLAST PENDING TOOL: {name}");
    }
    if let Some(summary) = &hint.last_pending_tool_input_summary {
        let _ = writeln!(out, "LAST PENDING INPUT SUMMARY: {summary}");
    }

    out.push_str(
        "\nSplit this task into 2-5 self-contained subtasks. Each subtask must:\n\
         - be implementable in isolation,\n\
         - have its own acceptance criteria,\n\
         - and together satisfy the parent's acceptance.\n\
         \n\
         Return EXACTLY this JSON shape (no commentary, no markdown fences):\n\
         {\n  \"reasoning\": \"<one paragraph explaining the split>\",\n  \
         \"subtasks\": [\n    {\n      \"id\": \"1\",\n      \
         \"description\": \"...\",\n      \"acceptance\": \"...\",\n      \
         \"depends_on\": []\n    }\n  ]\n}\n\
         \n\
         `depends_on` is a list of sibling subtask `id` values that must complete first.\n",
    );
    out
}

const SPLITTER_SYSTEM_PROMPT: &str = "\
You are a task splitter for a coding agent's dev loop. Given a stuck \
parent task (the agent explored but never wrote files) plus a structured \
failure hint, split the parent into 2-5 self-contained subtasks. Each \
subtask must be implementable in isolation, must have its own acceptance \
criteria, and together they must satisfy the parent's acceptance. \
Respond with JSON only (no commentary, no markdown fences). The JSON \
shape is documented in the user message.";

// ---------------------------------------------------------------------------
// Model call
// ---------------------------------------------------------------------------

async fn call_splitter(
    input: &DecompositionInput<'_>,
    user_prompt: String,
) -> Result<aura_reasoner::ModelResponse, DecompositionError> {
    let request = ModelRequest::builder(input.model.clone(), SPLITTER_SYSTEM_PROMPT)
        .message(Message::user(user_prompt))
        .tools(Vec::new())
        .tool_choice(ToolChoice::None)
        .max_tokens(DECOMPOSITION_MAX_TOKENS)
        .auth_token(input.auth_token.clone())
        .aura_project_id(input.aura_project_id.clone())
        .aura_agent_id(input.aura_agent_id.clone())
        .aura_session_id(input.aura_session_id.clone())
        .aura_org_id(input.aura_org_id.clone())
        .request_kind(ModelRequestKind::DevLoopBootstrap)
        .try_build()
        .map_err(|e| DecompositionError::Model(format!("invalid request: {e}")))?;
    input
        .provider
        .complete(request)
        .await
        .map_err(|e| DecompositionError::Model(e.to_string()))
}

fn extract_text(message: &Message) -> Option<String> {
    let mut buf = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text } = block {
            buf.push_str(text);
        }
    }
    if buf.trim().is_empty() {
        None
    } else {
        Some(buf)
    }
}

// ---------------------------------------------------------------------------
// Response parsing + validation
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize)]
struct ParsedSubtask {
    id: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    acceptance: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ParsedResponse {
    #[serde(default)]
    reasoning: String,
    subtasks: Vec<ParsedSubtask>,
}

/// Extract the first balanced JSON object from `raw_text` and parse
/// it as a [`ParsedResponse`]. Tolerates leading prose or accidental
/// markdown fences (`\`\`\`json ... \`\`\``) — we strip them and
/// scan for the first `{ ... }` block.
fn parse_response(raw_text: &str) -> Result<ParsedResponse, DecompositionError> {
    let cleaned = strip_code_fence(raw_text.trim());
    let json_slice = extract_json_object(cleaned).ok_or_else(|| {
        DecompositionError::InvalidShape("no JSON object found in model response".into())
    })?;
    let parsed: ParsedResponse = serde_json::from_str(json_slice)
        .map_err(|e| DecompositionError::InvalidShape(format!("json parse error: {e}")))?;
    validate_parsed(&parsed)?;
    Ok(parsed)
}

fn strip_code_fence(s: &str) -> &str {
    let trimmed = s.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
        return rest;
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        let rest = rest.trim_start();
        if let Some(inner) = rest.strip_suffix("```") {
            return inner.trim();
        }
        return rest;
    }
    trimmed
}

/// Return the slice of `s` covering the first balanced `{...}` block.
/// Honours string literals (a `{` inside a `"..."` string doesn't open
/// a new block) and escapes (`\"`).
fn extract_json_object(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn validate_parsed(parsed: &ParsedResponse) -> Result<(), DecompositionError> {
    let n = parsed.subtasks.len();
    if n < MIN_SUBTASKS_PER_DECOMPOSITION {
        return Err(DecompositionError::TooFewSubtasks);
    }
    if n > MAX_SUBTASKS_PER_DECOMPOSITION {
        return Err(DecompositionError::TooManySubtasks);
    }

    let mut seen: HashSet<&str> = HashSet::new();
    for st in &parsed.subtasks {
        let id_trim = st.id.trim();
        if id_trim.is_empty() {
            return Err(DecompositionError::InvalidShape(
                "subtask missing id".into(),
            ));
        }
        if !seen.insert(id_trim) {
            return Err(DecompositionError::InvalidShape(format!(
                "duplicate subtask id: {id_trim}"
            )));
        }
        if st.description.trim().is_empty() {
            return Err(DecompositionError::InvalidShape(format!(
                "subtask {id_trim} missing description"
            )));
        }
    }

    // depends_on validity
    let id_set: HashSet<&str> = parsed.subtasks.iter().map(|s| s.id.trim()).collect();
    for st in &parsed.subtasks {
        for dep in &st.depends_on {
            let dep_trim = dep.trim();
            if dep_trim == st.id.trim() {
                return Err(DecompositionError::InvalidShape(format!(
                    "subtask {} depends on itself",
                    st.id
                )));
            }
            if !id_set.contains(dep_trim) {
                return Err(DecompositionError::InvalidShape(format!(
                    "subtask {} depends on unknown sibling {dep_trim}",
                    st.id
                )));
            }
        }
    }

    // cycle check via Kahn's algorithm
    if has_cycle(&parsed.subtasks) {
        return Err(DecompositionError::Cyclic);
    }

    Ok(())
}

fn has_cycle(subtasks: &[ParsedSubtask]) -> bool {
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for st in subtasks {
        in_degree.entry(st.id.trim()).or_insert(0);
        adj.entry(st.id.trim()).or_default();
    }
    for st in subtasks {
        for dep in &st.depends_on {
            // edge: dep -> st (st depends on dep, so dep must be processed first)
            adj.entry(dep.trim()).or_default().push(st.id.trim());
            *in_degree.entry(st.id.trim()).or_insert(0) += 1;
        }
    }
    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut visited = 0usize;
    while let Some(node) = queue.pop() {
        visited += 1;
        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                if let Some(deg) = in_degree.get_mut(next) {
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(next);
                    }
                }
            }
        }
    }
    visited != in_degree.len()
}

// ---------------------------------------------------------------------------
// Conversion to TaskDescriptor
// ---------------------------------------------------------------------------

fn convert_to_descriptors(
    parent: &TaskDescriptor,
    parsed: &ParsedResponse,
) -> Result<Vec<TaskDescriptor>, DecompositionError> {
    // Build a model_id -> canonical_id mapping FIRST so dependency
    // rewrites are stable regardless of array ordering.
    let mut id_map: HashMap<&str, String> = HashMap::new();
    for (idx, st) in parsed.subtasks.iter().enumerate() {
        id_map.insert(st.id.trim(), canonical_subtask_id(&parent.id, idx));
    }

    let mut out = Vec::with_capacity(parsed.subtasks.len());
    for (idx, st) in parsed.subtasks.iter().enumerate() {
        let canonical_id = canonical_subtask_id(&parent.id, idx);
        let dependencies = st
            .depends_on
            .iter()
            .filter_map(|d| id_map.get(d.trim().to_string().as_str()).cloned())
            .collect::<Vec<_>>();

        let title = subtask_title(st, idx);
        let description = subtask_description(&parent.id, st);

        out.push(TaskDescriptor {
            id: canonical_id,
            spec_id: parent.spec_id.clone(),
            project_id: parent.project_id.clone(),
            title,
            description,
            status: "ready".to_string(),
            dependencies,
            order: parent.order.saturating_mul(100).saturating_add(idx as u32),
        });
    }
    Ok(out)
}

/// Deterministic subtask id used as both the dev-loop queue id and
/// the canonical reference in `depends_on` rewrites. Retries of the
/// decomposition step itself produce the same children — there is no
/// random suffix, no UUID, no timestamp. The `+ 1` is a readability
/// choice (subtask ids start at `sub::1`, not `sub::0`).
fn canonical_subtask_id(parent_id: &str, idx: usize) -> String {
    format!("{parent_id}::sub::{}", idx + 1)
}

fn subtask_title(st: &ParsedSubtask, idx: usize) -> String {
    // Use the first sentence (up to ~80 chars) of the description as
    // the title when the model didn't supply one. Falls back to a
    // numbered label so the title is never empty.
    let first_line = st
        .description
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if first_line.is_empty() {
        format!("Subtask {}", idx + 1)
    } else {
        truncate(&first_line, 80)
    }
}

fn subtask_description(parent_id: &str, st: &ParsedSubtask) -> String {
    let mut out = String::new();
    out.push_str(st.description.trim());
    if !st.acceptance.trim().is_empty() {
        out.push_str("\n\nAcceptance:\n");
        out.push_str(st.acceptance.trim());
    }
    out.push_str("\n\nParent task: ");
    out.push_str(parent_id);
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut = 0usize;
    for (i, _) in s.char_indices().take(max) {
        cut = i;
    }
    // include the last full char captured
    let end = s[cut..].chars().next().map(|c| cut + c.len_utf8()).unwrap_or(cut);
    format!("{}…", &s[..end])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use async_trait::async_trait;
    use aura_agent::prompts::{SymbolHit, WorkspaceReader};
    use aura_reasoner::{
        ContentBlock as ContentBlockReexport, MockProvider, MockResponse, ProviderTrace,
        ReasonerError, Role, StopReason, Usage,
    };

    fn parent_task() -> TaskDescriptor {
        TaskDescriptor {
            id: "parent-7".to_string(),
            spec_id: "spec-1".to_string(),
            project_id: "proj-1".to_string(),
            title: "Implement Publisher::enqueue + spawn_driver".to_string(),
            description:
                "Wire `Publisher::enqueue` in crates/zero-network/src/publisher.rs to call \
                 `spawn_driver` in crates/zero-storage/src/outbox.rs. Reuse Outbox::enqueue_batch."
                    .to_string(),
            status: "failed".to_string(),
            dependencies: Vec::new(),
            order: 3,
        }
    }

    fn hint_with_failed_paths() -> DecompositionHint {
        DecompositionHint {
            failed_paths: vec![
                "crates/zero-network/src/publisher.rs".into(),
                "crates/zero-storage/src/outbox.rs".into(),
            ],
            last_pending_tool_name: Some("write_file".into()),
            last_pending_tool_input_summary: Some(
                "{\"path\":\"crates/zero-network/src/publisher.rs\"}".into(),
            ),
        }
    }

    /// In-memory [`WorkspaceReader`] stub. Mirrors the one in
    /// `prompts::enrichment::tests` so the decomposition tests don't
    /// have to touch the real filesystem.
    #[derive(Default)]
    struct StubWorkspace {
        files: Mutex<std::collections::HashMap<String, String>>,
        definitions: Mutex<std::collections::HashMap<String, Vec<SymbolHit>>>,
    }

    impl StubWorkspace {
        fn with_file(self, path: &str, body: &str) -> Self {
            self.files
                .lock()
                .unwrap()
                .insert(path.to_string(), body.to_string());
            self
        }
    }

    #[async_trait]
    impl WorkspaceReader for StubWorkspace {
        async fn exists(&self, relative_path: &str) -> bool {
            self.files.lock().unwrap().contains_key(relative_path)
        }
        async fn read_file_head(
            &self,
            relative_path: &str,
            max_lines: usize,
        ) -> Option<String> {
            self.files.lock().unwrap().get(relative_path).map(|body| {
                body.lines()
                    .take(max_lines)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
            self.definitions
                .lock()
                .unwrap()
                .get(symbol)
                .cloned()
                .map(|v| v.into_iter().take(max_hits).collect())
                .unwrap_or_default()
        }
    }

    /// Recording model provider — captures every request it receives
    /// so tests can inspect the prompt that `auto_decompose_task`
    /// built. Returns the configured `MockResponse` on `complete`.
    struct RecordingProvider {
        captured: Mutex<Vec<ModelRequest>>,
        response: MockResponse,
    }

    impl RecordingProvider {
        fn new(response: MockResponse) -> Self {
            Self {
                captured: Mutex::new(Vec::new()),
                response,
            }
        }

        fn last_user_text(&self) -> String {
            let guard = self.captured.lock().unwrap();
            let request = guard.last().expect("at least one captured request");
            request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .flat_map(|m| m.content.iter())
                .filter_map(|b| match b {
                    ContentBlockReexport::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn last_system(&self) -> String {
            let guard = self.captured.lock().unwrap();
            guard.last().expect("captured").system.clone()
        }
    }

    #[async_trait]
    impl ModelProvider for RecordingProvider {
        fn name(&self) -> &'static str {
            "recording"
        }

        async fn complete(
            &self,
            request: ModelRequest,
        ) -> Result<aura_reasoner::ModelResponse, ReasonerError> {
            self.captured.lock().unwrap().push(request);
            Ok(aura_reasoner::ModelResponse::new(
                self.response.stop_reason,
                aura_reasoner::Message {
                    role: Role::Assistant,
                    content: self.response.content.clone(),
                },
                self.response.usage.clone(),
                ProviderTrace::new("recording-model", 0),
            ))
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    fn json_response_with(subtasks: &[(&str, &str, &[&str])], reasoning: &str) -> MockResponse {
        let st_json: Vec<serde_json::Value> = subtasks
            .iter()
            .map(|(id, desc, deps)| {
                serde_json::json!({
                    "id": id,
                    "description": desc,
                    "acceptance": format!("Acceptance for {id}"),
                    "depends_on": deps.iter().map(|d| (*d).to_string()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let body = serde_json::json!({
            "reasoning": reasoning,
            "subtasks": st_json,
        });
        MockResponse {
            stop_reason: StopReason::EndTurn,
            content: vec![ContentBlock::text(body.to_string())],
            usage: Usage::new(50, 50),
        }
    }

    fn make_input<'a>(
        parent: &'a TaskDescriptor,
        hint: &'a DecompositionHint,
        workspace: &'a dyn WorkspaceReader,
        provider: &'a dyn ModelProvider,
    ) -> DecompositionInput<'a> {
        DecompositionInput {
            parent_task: parent,
            hint,
            workspace,
            provider,
            model: "claude-opus-4-6".to_string(),
            auth_token: None,
            aura_org_id: None,
            aura_agent_id: None,
            aura_session_id: None,
            aura_project_id: None,
        }
    }

    #[tokio::test]
    async fn auto_decompose_validates_minimum_subtask_count() {
        // Model returns ONE subtask — must be rejected as
        // `TooFewSubtasks` so the caller falls back to task_failed.
        let provider = MockProvider::new().with_response(json_response_with(
            &[("1", "Only subtask", &[])],
            "trivial split",
        ));
        let workspace = StubWorkspace::default();
        let parent = parent_task();
        let hint = hint_with_failed_paths();
        let result =
            auto_decompose_task(make_input(&parent, &hint, &workspace, &provider)).await;
        assert!(
            matches!(result, Err(DecompositionError::TooFewSubtasks)),
            "expected TooFewSubtasks, got {result:?}"
        );
    }

    #[tokio::test]
    async fn auto_decompose_rejects_cyclic_dependencies() {
        // A -> B -> A cycle in depends_on must be refused.
        let provider = MockProvider::new().with_response(json_response_with(
            &[
                ("A", "Subtask A", &["B"]),
                ("B", "Subtask B", &["A"]),
            ],
            "cyclic split",
        ));
        let workspace = StubWorkspace::default();
        let parent = parent_task();
        let hint = hint_with_failed_paths();
        let result =
            auto_decompose_task(make_input(&parent, &hint, &workspace, &provider)).await;
        assert!(
            matches!(result, Err(DecompositionError::Cyclic)),
            "expected Cyclic, got {result:?}"
        );
    }

    #[tokio::test]
    async fn auto_decompose_assigns_deterministic_subtask_ids() {
        // Same input -> same canonical subtask ids. Run twice and
        // assert the id vectors are bitwise-equal so retries don't
        // orphan in-flight subtask state.
        let provider1 = MockProvider::new().with_response(json_response_with(
            &[
                ("1", "Subtask 1", &[]),
                ("2", "Subtask 2", &["1"]),
                ("3", "Subtask 3", &["1", "2"]),
            ],
            "ordered split",
        ));
        let provider2 = MockProvider::new().with_response(json_response_with(
            &[
                ("1", "Subtask 1", &[]),
                ("2", "Subtask 2", &["1"]),
                ("3", "Subtask 3", &["1", "2"]),
            ],
            "ordered split",
        ));
        let workspace = StubWorkspace::default();
        let parent = parent_task();
        let hint = hint_with_failed_paths();

        let first = auto_decompose_task(make_input(&parent, &hint, &workspace, &provider1))
            .await
            .expect("first decomposition succeeds");
        let second = auto_decompose_task(make_input(&parent, &hint, &workspace, &provider2))
            .await
            .expect("second decomposition succeeds");

        let first_ids: Vec<String> = first.subtasks.iter().map(|t| t.id.clone()).collect();
        let second_ids: Vec<String> = second.subtasks.iter().map(|t| t.id.clone()).collect();
        assert_eq!(first_ids, second_ids);
        assert_eq!(first_ids[0], "parent-7::sub::1");
        assert_eq!(first_ids[1], "parent-7::sub::2");
        assert_eq!(first_ids[2], "parent-7::sub::3");

        // depends_on are rewritten to the canonical form.
        assert!(first.subtasks[1].dependencies.contains(&"parent-7::sub::1".to_string()));
        assert!(first.subtasks[2].dependencies.contains(&"parent-7::sub::1".to_string()));
        assert!(first.subtasks[2].dependencies.contains(&"parent-7::sub::2".to_string()));
    }

    #[tokio::test]
    async fn auto_decompose_includes_resolved_context_in_prompt() {
        // The Phase 4 pre-resolved context block must show up in the
        // splitter's user prompt so the model can see the same
        // signal the parent task got on its first attempt.
        let provider = RecordingProvider::new(json_response_with(
            &[
                ("1", "Wire Publisher::enqueue", &[]),
                ("2", "Add spawn_driver", &["1"]),
            ],
            "split based on file boundaries",
        ));
        let workspace = StubWorkspace::default()
            .with_file(
                "crates/zero-storage/src/outbox.rs",
                "pub struct Outbox;\n\nimpl Outbox {\n    pub fn enqueue_batch(&self) {}\n}\n",
            )
            .with_file(
                "crates/zero-network/src/publisher.rs",
                "pub struct Publisher;\n",
            );
        let parent = parent_task();
        let hint = hint_with_failed_paths();

        let result = auto_decompose_task(make_input(&parent, &hint, &workspace, &provider))
            .await
            .expect("succeeds with stub response");
        assert_eq!(result.subtasks.len(), 2);

        let user_prompt = provider.last_user_text();
        assert!(
            user_prompt.contains("crates/zero-storage/src/outbox.rs"),
            "user prompt must reference resolved path, got:\n{user_prompt}"
        );
        assert!(
            user_prompt.contains("pub struct Outbox"),
            "user prompt must include resolved file head, got:\n{user_prompt}"
        );
        assert!(
            user_prompt.contains("FAILED PATHS"),
            "user prompt must echo the failed-paths hint, got:\n{user_prompt}"
        );
        assert!(
            user_prompt.contains("PARENT TASK: Implement Publisher::enqueue + spawn_driver"),
            "user prompt must name the parent task, got:\n{user_prompt}"
        );

        let system = provider.last_system();
        assert!(
            system.contains("task splitter"),
            "splitter system prompt must self-identify, got:\n{system}"
        );
    }

    #[test]
    fn parse_response_strips_markdown_fence() {
        let raw = "```json\n{\"reasoning\":\"x\",\"subtasks\":[\
                   {\"id\":\"1\",\"description\":\"a\",\"acceptance\":\"\",\"depends_on\":[]},\
                   {\"id\":\"2\",\"description\":\"b\",\"acceptance\":\"\",\"depends_on\":[]}\
                   ]}\n```";
        let parsed = parse_response(raw).expect("must parse fenced JSON");
        assert_eq!(parsed.subtasks.len(), 2);
    }

    #[test]
    fn parse_response_rejects_too_many_subtasks() {
        let mut subtasks = String::new();
        for i in 0..(MAX_SUBTASKS_PER_DECOMPOSITION + 1) {
            if i > 0 {
                subtasks.push(',');
            }
            subtasks.push_str(&format!(
                "{{\"id\":\"{i}\",\"description\":\"d{i}\",\"acceptance\":\"\",\"depends_on\":[]}}"
            ));
        }
        let raw = format!("{{\"reasoning\":\"too many\",\"subtasks\":[{subtasks}]}}");
        let err = parse_response(&raw).expect_err("expected error");
        assert!(matches!(err, DecompositionError::TooManySubtasks));
    }

    #[test]
    fn parse_response_rejects_unknown_depends_on() {
        let raw = r#"{"reasoning":"x","subtasks":[
            {"id":"1","description":"a","acceptance":"","depends_on":[]},
            {"id":"2","description":"b","acceptance":"","depends_on":["999"]}
        ]}"#;
        let err = parse_response(raw).expect_err("expected error");
        match err {
            DecompositionError::InvalidShape(msg) => {
                assert!(msg.contains("unknown sibling"), "msg: {msg}");
            }
            other => panic!("expected InvalidShape, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_rejects_self_dependency() {
        let raw = r#"{"reasoning":"x","subtasks":[
            {"id":"1","description":"a","acceptance":"","depends_on":["1"]},
            {"id":"2","description":"b","acceptance":"","depends_on":[]}
        ]}"#;
        let err = parse_response(raw).expect_err("expected error");
        match err {
            DecompositionError::InvalidShape(msg) => {
                assert!(msg.contains("itself"), "msg: {msg}");
            }
            other => panic!("expected InvalidShape, got {other:?}"),
        }
    }

    #[test]
    fn canonical_subtask_id_is_deterministic() {
        assert_eq!(canonical_subtask_id("parent", 0), "parent::sub::1");
        assert_eq!(canonical_subtask_id("parent", 4), "parent::sub::5");
    }
}
