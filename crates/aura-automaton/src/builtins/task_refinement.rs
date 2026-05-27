//! Pre-implementation task description refinement.
//!
//! Both the dev-loop and single-task automatons converge on this
//! helper after they have loaded the [`TaskDescriptor`] and the
//! [`SpecDescriptor`] but before they call
//! `AgentRunner::execute_task_tracked`. The helper asks the configured
//! [`ModelProvider`] for a description that more precisely matches the
//! spec's intent, persists it via [`DomainApi::update_task`], and
//! returns the updated descriptor so the caller can continue with the
//! refined text in `AgenticTaskParams`.
//!
//! Behaviour contract (locked in by the unit tests in this module):
//!
//! 1. If the incoming `task.description` already starts with
//!    [`REFINED_MARKER`] we return immediately with `task.clone()` —
//!    no provider call, no `update_task`, no events. This is the
//!    idempotency guard that combines with the dev-loop
//!    `build_retry_note.is_none()` gate to ensure refinement runs at
//!    most once per task.
//! 2. We emit [`AutomatonEvent::TaskDescriptionRefining`] on a
//!    best-effort basis (`event_tx` may be `None`, send failures are
//!    swallowed; observability is nice-to-have, never blocking).
//! 3. On provider/`update_task` failure we emit a `LogLine` and fall
//!    through to `Ok(task.clone())`. Refinement is purely a
//!    pre-implementation aid; if it fails we still deliver the task
//!    with the operator-authored description so the agent loop keeps
//!    making forward progress.
//! 4. On success we wrap the model output in a stable, traceable shape
//!    that preserves the original task block verbatim and persists it
//!    via `domain.update_task`. We then emit
//!    [`AutomatonEvent::TaskDescriptionRefined`] and return the
//!    updated descriptor returned by the domain API.

use aura_reasoner::ModelProvider;
use aura_tools::domain_tools::{DomainApi, SpecDescriptor, TaskDescriptor, TaskUpdate};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::common::{run_auxiliary_model_call, AuxiliaryModelCall};
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;

/// Idempotency marker prepended to every persisted refined
/// description. The leading-trim check in
/// [`refine_task_description`] uses this string verbatim so a future
/// schema bump can introduce `aura-refined:v2` without invalidating
/// existing rows (both versions will simply skip the second refinement
/// pass).
pub(crate) const REFINED_MARKER: &str = "<!-- aura-refined:v1 -->";

/// System prompt for the refinement call. Kept short on purpose —
/// the user message carries the spec / task body and a single
/// formatting instruction, so the system prompt only needs to set the
/// role.
const REFINEMENT_SYSTEM_PROMPT: &str =
    "You refine software engineering task descriptions before implementation. \
Rewrite the task description so it precisely matches the spec's intent and acceptance criteria. \
Keep it concise and actionable.";

/// Refine the description of `task` against `spec` via `provider`,
/// persist it through `domain`, and return the updated descriptor.
///
/// See module-level docs for the full behaviour contract. The
/// `model` parameter is the model identifier the underlying
/// `ModelRequest` will carry; both automatons already track their
/// configured model on the [`AgentRunner`] / dispatch JSON, so this
/// is plumbed in from the caller rather than re-derived inside the
/// helper.
///
/// Any failure (provider error, `update_task` error) is logged via
/// [`AutomatonEvent::LogLine`] and the original `task` is returned
/// unchanged. The function is therefore guaranteed not to error out
/// the upstream tick on refinement issues — refinement is best-effort.
pub(crate) async fn refine_task_description(
    domain: &dyn DomainApi,
    provider: &dyn ModelProvider,
    model: &str,
    spec: &SpecDescriptor,
    task: &TaskDescriptor,
    event_tx: Option<&mpsc::Sender<AutomatonEvent>>,
) -> Result<TaskDescriptor, AutomatonError> {
    // Idempotency: a previous run already persisted a refined body.
    // Skip silently — no provider call, no events. The dev-loop
    // `build_retry_note.is_none()` gate handles the same skip for
    // the build-retry second pass; this guard covers the case where
    // the task is re-claimed after a process restart.
    if task.description.trim_start().starts_with(REFINED_MARKER) {
        debug!(
            task_id = %task.id,
            "task description already carries refinement marker; skipping"
        );
        return Ok(task.clone());
    }

    emit_best_effort(
        event_tx,
        AutomatonEvent::TaskDescriptionRefining {
            task_id: task.id.clone(),
        },
    );

    let call = AuxiliaryModelCall {
        model,
        system_prompt: REFINEMENT_SYSTEM_PROMPT,
        user_body: build_refinement_user_body(spec, task),
        max_tokens: aura_config::agent().automaton.refinement_max_tokens,
        task_scope: Some(task.id.clone()),
    };
    let response = match run_auxiliary_model_call(provider, call).await {
        Ok(r) => r,
        Err(e) => {
            warn!(task_id = %task.id, error = %e, "auxiliary model call rejected refinement");
            emit_best_effort(
                event_tx,
                AutomatonEvent::LogLine {
                    message: format!("task description refinement failed for {}: {e}", task.id),
                },
            );
            return Ok(task.clone());
        }
    };

    let refined_text = response.message.text_content();
    let refined_trimmed = refined_text.trim();
    if refined_trimmed.is_empty() {
        // An empty rewrite is functionally no different from a
        // failure — the original description is strictly more
        // informative. Fall through with a LogLine instead of
        // persisting an empty body.
        emit_best_effort(
            event_tx,
            AutomatonEvent::LogLine {
                message: format!(
                    "task description refinement returned empty body for {}",
                    task.id
                ),
            },
        );
        return Ok(task.clone());
    }

    let refined_body = assemble_refined_body(refined_trimmed, task);

    let updated = match domain
        .update_task(
            &task.id,
            TaskUpdate {
                description: Some(refined_body),
                ..Default::default()
            },
            None,
        )
        .await
    {
        Ok(t) => t,
        Err(e) => {
            warn!(task_id = %task.id, error = %e, "update_task rejected refined description");
            emit_best_effort(
                event_tx,
                AutomatonEvent::LogLine {
                    message: format!("task description refinement failed for {}: {e}", task.id),
                },
            );
            return Ok(task.clone());
        }
    };

    emit_best_effort(
        event_tx,
        AutomatonEvent::TaskDescriptionRefined {
            task_id: task.id.clone(),
        },
    );

    Ok(updated)
}

/// Build the user-message body for the refinement call. Mirrors the
/// shape used by `aura_automaton::builtins::spec_gen` so the
/// auxiliary-call template in
/// [`super::common::run_auxiliary_model_call`] stays the single seam
/// where the `ModelRequest::builder(...)` knobs (`max_tokens`,
/// `tool_choice`, etc.) are picked.
fn build_refinement_user_body(spec: &SpecDescriptor, task: &TaskDescriptor) -> String {
    format!(
        "# Spec: {spec_title}\n\n{spec_content}\n\n\
         # Current Task: {task_title}\n\n{task_desc}\n\n\
         # Output\nReturn only the refined task description in markdown, no preamble.",
        spec_title = spec.title,
        spec_content = spec.content,
        task_title = task.title,
        task_desc = task.description,
    )
}

/// Wrap the model output and the original task block in the stable
/// shape persisted by [`refine_task_description`]. Exposed for tests
/// so the format can be asserted directly without round-tripping the
/// whole helper.
fn assemble_refined_body(refined: &str, task: &TaskDescriptor) -> String {
    let mut out = String::with_capacity(refined.len() + task.description.len() + 256);
    out.push_str(REFINED_MARKER);
    out.push('\n');
    out.push_str("## Refined Description\n");
    out.push_str(refined);
    out.push_str("\n\n## Original Task\n");
    out.push_str("> ");
    out.push_str(&task.title);
    out.push('\n');
    // Preserve a blank-quoted line between title and body so the
    // rendered markdown shows a paragraph break inside the blockquote.
    out.push_str(">\n");
    if task.description.is_empty() {
        out.push_str(">\n");
    } else {
        for line in task.description.lines() {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Send an event without failing the caller. The two callers expect
/// `mpsc::Sender::try_send` semantics, but we use the async `send`
/// here for resilience to a transiently full channel. A closed
/// channel is a no-op — observability is best-effort.
fn emit_best_effort(event_tx: Option<&mpsc::Sender<AutomatonEvent>>, event: AutomatonEvent) {
    let Some(tx) = event_tx else { return };
    if let Err(e) = tx.try_send(event) {
        debug!(error = ?e, "task_refinement event drop (channel full or closed)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use anyhow::anyhow;
    use async_trait::async_trait;
    use aura_reasoner::{
        ContentBlock, Message as ReasonerMessage, ModelRequest, ModelResponse, ProviderTrace,
        ReasonerError, Role, StopReason, Usage,
    };
    use aura_tools::domain_tools::{
        CreateSessionParams, DomainApi, MessageDescriptor, ProjectDescriptor, ProjectUpdate,
        SaveMessageParams, SessionDescriptor, SpecDescriptor, TaskDescriptor, TaskUpdate,
    };

    // ------------------------------------------------------------
    // Mock ModelProvider that counts calls and lets the test inject
    // either a canned text response or an error.
    // ------------------------------------------------------------

    struct CountingProvider {
        outcome: Mutex<Vec<Result<String, &'static str>>>,
        calls: AtomicUsize,
    }

    impl CountingProvider {
        fn with_text(text: &str) -> Self {
            Self {
                outcome: Mutex::new(vec![Ok(text.to_string())]),
                calls: AtomicUsize::new(0),
            }
        }

        fn with_error() -> Self {
            Self {
                outcome: Mutex::new(vec![Err("simulated provider failure")]),
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ModelProvider for CountingProvider {
        fn name(&self) -> &'static str {
            "test-counting-provider"
        }

        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = {
                let mut q = self.outcome.lock().unwrap();
                if q.is_empty() {
                    Ok("default refined body".to_string())
                } else {
                    q.remove(0)
                }
            };
            match next {
                Ok(text) => Ok(ModelResponse {
                    stop_reason: StopReason::EndTurn,
                    message: ReasonerMessage {
                        role: Role::Assistant,
                        content: vec![ContentBlock::text(text)],
                    },
                    usage: Usage::new(10, 20),
                    trace: ProviderTrace::new("test-model", 0),
                    model_used: "test-model".to_string(),
                }),
                Err(msg) => Err(ReasonerError::Internal(msg.to_string())),
            }
        }

        async fn health_check(&self) -> bool {
            true
        }
    }

    // ------------------------------------------------------------
    // Mock DomainApi: every method `unimplemented!()` except
    // `update_task`, which records its inputs and returns a
    // descriptor with the description we received. Mirrors the
    // `UnusedDomain` pattern in
    // `crates/aura-runtime/src/automaton_bridge/tests.rs`.
    // ------------------------------------------------------------

    #[derive(Default)]
    struct RecordingDomain {
        updates: Mutex<Vec<(String, TaskUpdate)>>,
        update_should_fail: bool,
    }

    impl RecordingDomain {
        fn updates(&self) -> Vec<(String, TaskUpdate)> {
            self.updates.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DomainApi for RecordingDomain {
        async fn list_specs(
            &self,
            _project_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<Vec<SpecDescriptor>> {
            unimplemented!("RecordingDomain")
        }
        async fn get_spec(
            &self,
            _spec_id: &str,
            _jwt: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn create_spec(
            &self,
            _p: &str,
            _t: &str,
            _c: &str,
            _o: u32,
            _j: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn update_spec(
            &self,
            _id: &str,
            _t: Option<&str>,
            _c: Option<&str>,
            _j: Option<&str>,
        ) -> anyhow::Result<SpecDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn delete_spec(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<()> {
            unimplemented!("RecordingDomain")
        }
        async fn list_tasks(
            &self,
            _p: &str,
            _s: Option<&str>,
            _j: Option<&str>,
        ) -> anyhow::Result<Vec<TaskDescriptor>> {
            unimplemented!("RecordingDomain")
        }
        async fn create_task(
            &self,
            _p: &str,
            _s: &str,
            _t: &str,
            _d: &str,
            _deps: &[String],
            _o: u32,
            _j: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn update_task(
            &self,
            id: &str,
            updates: TaskUpdate,
            _jwt: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            self.updates
                .lock()
                .unwrap()
                .push((id.to_string(), updates.clone()));
            if self.update_should_fail {
                return Err(anyhow!("simulated update_task failure"));
            }
            let new_desc = updates.description.clone().unwrap_or_default();
            Ok(TaskDescriptor {
                id: id.to_string(),
                spec_id: String::new(),
                project_id: String::new(),
                title: String::new(),
                description: new_desc,
                status: String::new(),
                dependencies: Vec::new(),
                order: 0,
            })
        }
        async fn delete_task(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<()> {
            unimplemented!("RecordingDomain")
        }
        async fn transition_task(
            &self,
            _id: &str,
            _s: &str,
            _j: Option<&str>,
        ) -> anyhow::Result<TaskDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn claim_next_task(
            &self,
            _p: &str,
            _a: &str,
            _j: Option<&str>,
        ) -> anyhow::Result<Option<TaskDescriptor>> {
            unimplemented!("RecordingDomain")
        }
        async fn get_task(&self, _id: &str, _j: Option<&str>) -> anyhow::Result<TaskDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn get_project(
            &self,
            _p: &str,
            _j: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn update_project(
            &self,
            _p: &str,
            _u: ProjectUpdate,
            _j: Option<&str>,
        ) -> anyhow::Result<ProjectDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn create_log(
            &self,
            _p: &str,
            _m: &str,
            _l: &str,
            _a: Option<&str>,
            _md: Option<&serde_json::Value>,
            _j: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            unimplemented!("RecordingDomain")
        }
        async fn list_logs(
            &self,
            _p: &str,
            _l: Option<&str>,
            _n: Option<u64>,
            _j: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            unimplemented!("RecordingDomain")
        }
        async fn get_project_stats(
            &self,
            _p: &str,
            _j: Option<&str>,
        ) -> anyhow::Result<serde_json::Value> {
            unimplemented!("RecordingDomain")
        }
        async fn list_messages(
            &self,
            _p: &str,
            _i: &str,
        ) -> anyhow::Result<Vec<MessageDescriptor>> {
            unimplemented!("RecordingDomain")
        }
        async fn save_message(&self, _p: SaveMessageParams) -> anyhow::Result<()> {
            unimplemented!("RecordingDomain")
        }
        async fn create_session(
            &self,
            _p: CreateSessionParams,
        ) -> anyhow::Result<SessionDescriptor> {
            unimplemented!("RecordingDomain")
        }
        async fn get_active_session(&self, _i: &str) -> anyhow::Result<Option<SessionDescriptor>> {
            unimplemented!("RecordingDomain")
        }
        async fn orbit_api_call(
            &self,
            _m: &str,
            _p: &str,
            _b: Option<&serde_json::Value>,
            _j: Option<&str>,
        ) -> anyhow::Result<String> {
            unimplemented!("RecordingDomain")
        }
        async fn network_api_call(
            &self,
            _m: &str,
            _p: &str,
            _b: Option<&serde_json::Value>,
            _j: Option<&str>,
        ) -> anyhow::Result<String> {
            unimplemented!("RecordingDomain")
        }
    }

    fn sample_spec() -> SpecDescriptor {
        SpecDescriptor {
            id: "spec-1".into(),
            project_id: "proj-1".into(),
            title: "Auth service".into(),
            content: "Implement token-based auth with rotating refresh tokens.".into(),
            order: 0,
            parent_id: None,
        }
    }

    fn sample_task(description: &str) -> TaskDescriptor {
        TaskDescriptor {
            id: "task-1".into(),
            spec_id: "spec-1".into(),
            project_id: "proj-1".into(),
            title: "Wire refresh-token rotation".into(),
            description: description.into(),
            status: "ready".into(),
            dependencies: Vec::new(),
            order: 0,
        }
    }

    fn drain(rx: &mut mpsc::Receiver<AutomatonEvent>) -> Vec<AutomatonEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[tokio::test]
    async fn skips_when_marker_present() {
        let domain = RecordingDomain::default();
        let provider = CountingProvider::with_text("should never run");
        let (tx, mut rx) = mpsc::channel::<AutomatonEvent>(8);
        let spec = sample_spec();
        let task = sample_task("<!-- aura-refined:v1 -->\n## Refined Description\nalready done");

        let out =
            refine_task_description(&domain, &provider, "test-model", &spec, &task, Some(&tx))
                .await
                .expect("marker short-circuit must succeed");

        assert_eq!(
            out.description, task.description,
            "marker short-circuit must return the input description unchanged"
        );
        assert_eq!(
            provider.calls(),
            0,
            "marker short-circuit must not call the provider"
        );
        assert!(
            domain.updates().is_empty(),
            "marker short-circuit must not call update_task"
        );
        assert!(
            drain(&mut rx).is_empty(),
            "marker short-circuit must emit no events"
        );
    }

    #[tokio::test]
    async fn falls_back_on_provider_error() {
        let domain = RecordingDomain::default();
        let provider = CountingProvider::with_error();
        let (tx, mut rx) = mpsc::channel::<AutomatonEvent>(8);
        let spec = sample_spec();
        let task = sample_task("Make the tokens rotate every 15 minutes.");

        let out =
            refine_task_description(&domain, &provider, "test-model", &spec, &task, Some(&tx))
                .await
                .expect("provider failure must not error the caller");

        assert_eq!(
            out.description, task.description,
            "provider failure must return the original description"
        );
        assert!(
            domain.updates().is_empty(),
            "provider failure must not persist anything"
        );

        let events = drain(&mut rx);
        let mut saw_refining = false;
        let mut saw_log = false;
        let mut saw_refined = false;
        for e in &events {
            match e {
                AutomatonEvent::TaskDescriptionRefining { task_id } => {
                    assert_eq!(task_id, "task-1");
                    saw_refining = true;
                }
                AutomatonEvent::LogLine { message } => {
                    assert!(
                        message.contains("task-1") && message.contains("refinement failed"),
                        "LogLine must carry the failure context, got: {message}"
                    );
                    saw_log = true;
                }
                AutomatonEvent::TaskDescriptionRefined { .. } => {
                    saw_refined = true;
                }
                other => panic!("unexpected event on provider-failure path: {other:?}"),
            }
        }
        assert!(
            saw_refining,
            "refining event must still fire before the call"
        );
        assert!(saw_log, "exactly one LogLine must capture the failure");
        assert!(
            !saw_refined,
            "the success event must NOT fire when the provider rejected the call"
        );
    }

    #[tokio::test]
    async fn persists_refined_description() {
        let domain = RecordingDomain::default();
        let provider = CountingProvider::with_text(
            "Rotate refresh tokens every 15 minutes and revoke prior keys.",
        );
        let (tx, mut rx) = mpsc::channel::<AutomatonEvent>(8);
        let spec = sample_spec();
        let task = sample_task("Rotate tokens.");

        let out =
            refine_task_description(&domain, &provider, "test-model", &spec, &task, Some(&tx))
                .await
                .expect("happy path must succeed");

        assert!(
            out.description.starts_with(REFINED_MARKER),
            "returned description must carry the marker, got: {}",
            out.description
        );
        assert!(
            out.description.contains("## Original Task"),
            "returned description must preserve the original task block"
        );
        assert!(
            out.description.contains("> Rotate tokens."),
            "original description must be quoted line-by-line"
        );
        assert!(
            out.description
                .contains("Rotate refresh tokens every 15 minutes"),
            "refined model output must be embedded in the body"
        );

        let updates = domain.updates();
        assert_eq!(updates.len(), 1, "update_task must be called exactly once");
        let (id, update) = &updates[0];
        assert_eq!(id, "task-1");
        let persisted = update
            .description
            .as_deref()
            .expect("update payload must carry the new description");
        assert!(persisted.starts_with(REFINED_MARKER));
        assert!(persisted.contains("## Original Task"));

        let events = drain(&mut rx);
        let mut saw_refining = false;
        let mut saw_refined = false;
        for e in &events {
            match e {
                AutomatonEvent::TaskDescriptionRefining { task_id } => {
                    assert_eq!(task_id, "task-1");
                    saw_refining = true;
                }
                AutomatonEvent::TaskDescriptionRefined { task_id } => {
                    assert_eq!(task_id, "task-1");
                    saw_refined = true;
                }
                AutomatonEvent::LogLine { message } => {
                    panic!("happy path must not emit a LogLine, got: {message}")
                }
                other => panic!("unexpected event on happy path: {other:?}"),
            }
        }
        assert!(saw_refining, "TaskDescriptionRefining must fire");
        assert!(saw_refined, "TaskDescriptionRefined must fire on success");
    }

    #[tokio::test]
    async fn empty_provider_output_falls_back_to_original() {
        // A whitespace-only model response is functionally indistinguishable
        // from a failure — the original description is strictly more
        // informative. Lock in the fall-through.
        let domain = RecordingDomain::default();
        let provider = CountingProvider::with_text("   \n\t ");
        let (tx, mut rx) = mpsc::channel::<AutomatonEvent>(8);
        let spec = sample_spec();
        let task = sample_task("Rotate tokens.");

        let out =
            refine_task_description(&domain, &provider, "test-model", &spec, &task, Some(&tx))
                .await
                .expect("empty provider output must not error the caller");

        assert_eq!(out.description, task.description);
        assert!(
            domain.updates().is_empty(),
            "empty refinement must not persist anything"
        );
        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AutomatonEvent::LogLine { .. })),
            "empty refinement must emit a LogLine"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AutomatonEvent::TaskDescriptionRefined { .. })),
            "empty refinement must NOT emit the success event"
        );
    }

    #[tokio::test]
    async fn falls_back_on_update_task_error() {
        let mut domain = RecordingDomain::default();
        domain.update_should_fail = true;
        let provider = CountingProvider::with_text("Refined body.");
        let (tx, mut rx) = mpsc::channel::<AutomatonEvent>(8);
        let spec = sample_spec();
        let task = sample_task("Rotate tokens.");

        let out =
            refine_task_description(&domain, &provider, "test-model", &spec, &task, Some(&tx))
                .await
                .expect("update_task failure must not error the caller");

        assert_eq!(out.description, task.description);
        assert_eq!(
            provider.calls(),
            1,
            "provider must still have been called once"
        );
        assert_eq!(
            domain.updates().len(),
            1,
            "update_task is still attempted exactly once"
        );

        let events = drain(&mut rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AutomatonEvent::LogLine { .. })),
            "update_task failure must emit a LogLine"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AutomatonEvent::TaskDescriptionRefined { .. })),
            "update_task failure must NOT emit the success event"
        );
    }

    #[tokio::test]
    async fn works_without_event_channel() {
        // event_tx = None must not panic and must still run the
        // provider + update_task path.
        let domain = RecordingDomain::default();
        let provider = CountingProvider::with_text("Refined body.");
        let spec = sample_spec();
        let task = sample_task("Rotate tokens.");

        let out = refine_task_description(&domain, &provider, "test-model", &spec, &task, None)
            .await
            .expect("None event_tx must work");

        assert_eq!(provider.calls(), 1);
        assert_eq!(domain.updates().len(), 1);
        assert!(out.description.starts_with(REFINED_MARKER));
    }

    #[test]
    fn assemble_refined_body_quotes_multiline_descriptions() {
        let task = sample_task("Line one.\nLine two.\nLine three.");
        let body = assemble_refined_body("Refined body.", &task);
        assert!(body.starts_with(REFINED_MARKER));
        assert!(body.contains("## Refined Description\nRefined body."));
        assert!(body.contains("> Line one."));
        assert!(body.contains("> Line two."));
        assert!(body.contains("> Line three."));
    }
}
