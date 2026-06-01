//! AURA Council orchestrator.
//!
//! A council run fans the user's chat query across `members` in
//! parallel — one subagent child run per member — then runs a
//! synthesizer turn on the PARENT run (driven by `members[0]`, the
//! first model) that compares and reconciles every member's answer
//! into one combined response.
//!
//! Layer boundary: like the rest of `gateway::session`, this is the
//! only place council orchestration lives. It reuses the existing
//! primitives wholesale rather than re-implementing them:
//!
//! - The PARENT run is created + registered through the SAME
//!   [`super::chat_run::spawn_chat_run`] path a `POST /v1/run` chat run
//!   uses, so `WS /stream/:run_id` attaches non-destructively (history
//!   replay + live). The parent session is prepared with `members[0]`'s
//!   model, so the synthesizer turn runs on the right model for free.
//! - Members fan out through the SAME
//!   [`super::subagent_stream::RuntimeSubagentObservabilityHook`] +
//!   [`FleetSubagentDispatcher`] used for ordinary `task` spawns. Each
//!   member therefore runs the full real-agent loop, inheriting the
//!   parent identity / permissions, with only its model overridden, and
//!   streams live on its own child run stream.
//! - The synthesizer turn is dispatched by injecting a synthetic
//!   `user_message` into the parent run's command channel: the chat
//!   driver then runs a normal turn (`text_delta` …
//!   `assistant_message_end`) with `members[0]`'s model. No model calls
//!   are hand-rolled.
//!
//! Cancellation: the orchestrator forks every member's cancellation off
//! the PARENT run's `shutdown` token (via the observability hook's
//! `child_cancellation`), and the synthesizer turn runs inside the
//! parent driver which already cancels on `shutdown`. So a single
//! `POST /v1/run/:id/stop` (or a parent `Cancel`) aborts all in-flight
//! members AND the synthesizer.

use std::sync::Arc;
use std::time::Duration;

use aura_core_types::{
    AgentId, AgentPermissions, AgentToolPermissions, SubagentDispatchRequest, SubagentExit,
    SubagentResult, UserToolDefaults,
};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_protocol::{ConversationMessage, CouncilMember, RuntimeRequest, RuntimeRequestType};
use aura_tools::SubagentDispatchHook;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use super::chat_run::{spawn_event_forwarder, ChatEventChannel, ChatRunHandle, ChatRunRegistry};
use super::helpers::{build_fleet_subagent_dispatcher, prepare_chat_session, ChatRequestError};
use super::subagent_stream::{EventAwareSubagentDispatch, RuntimeSubagentObservabilityHook};
use super::WsContext;
use crate::protocol::{InboundMessage, OutboundMessage, UserMessage};

/// Default cap on council members when `AURA_COUNCIL_MAX_MEMBERS` is
/// unset / unparsable. Extra members beyond the cap are silently
/// truncated (with a warning).
const DEFAULT_COUNCIL_MAX_MEMBERS: usize = 4;

/// Bundled subagent kind each council member runs as. `general_purpose`
/// is the full multi-step agent loop (read/write/run tools), so a member
/// answers the query like a real agent rather than a read-only explorer.
const COUNCIL_MEMBER_KIND: &str = "general_purpose";

/// Parent-derived inputs every member dispatch inherits. Captured from
/// the prepared parent session before it is handed to the chat driver,
/// so members run with the parent's identity / permissions and only
/// their model overridden.
struct CouncilDispatchParams {
    parent_agent_id: AgentId,
    parent_permissions: AgentPermissions,
    parent_tool_permissions: Option<AgentToolPermissions>,
    user_tool_defaults: UserToolDefaults,
    originating_user_id: Option<String>,
    /// `members[0]` model id — stamped as the dispatch's
    /// `parent_model_id` so members inherit the synthesizer's model
    /// snapshot for billing/identity.
    parent_model_id: Option<String>,
}

/// Terminal outcome of a single council member's fan-out dispatch.
struct MemberOutcome {
    council_index: u32,
    model: Option<String>,
    result: Result<SubagentResult, String>,
}

impl MemberOutcome {
    /// The member's usable answer text, or an `Err(note)` describing why
    /// the member produced nothing the synthesizer can use (failure /
    /// rejection / cancellation / empty output / dispatch error).
    fn member_text(&self) -> Result<&str, String> {
        match &self.result {
            Ok(result) => match &result.exit {
                SubagentExit::Completed => {
                    if result.final_message.trim().is_empty() {
                        Err("member returned no content".to_string())
                    } else {
                        Ok(result.final_message.as_str())
                    }
                }
                SubagentExit::Failed { reason } => Err(format!("failed: {reason}")),
                SubagentExit::Rejected { reason } => Err(format!("rejected: {reason}")),
                SubagentExit::Cancelled => Err("cancelled".to_string()),
                SubagentExit::Timeout => Err("timed out".to_string()),
            },
            Err(err) => Err(format!("dispatch error: {err}")),
        }
    }

    fn is_success(&self) -> bool {
        self.member_text().is_ok()
    }
}

/// Everything the detached orchestrator task needs to drive a council
/// after the parent run is registered.
struct CouncilOrchestrator {
    handle: Arc<ChatRunHandle>,
    fleet_dispatcher: Arc<FleetSubagentDispatcher>,
    members: Vec<CouncilMember>,
    query: String,
    params: CouncilDispatchParams,
    registry: ChatRunRegistry,
    run_id: String,
    shutdown: CancellationToken,
}

/// Start an AURA Council run.
///
/// Mirrors [`super::chat_run::spawn_chat_run`]'s setup to create + register
/// the PARENT run (hosting the synthesizer, `members[0]`), then detaches
/// an orchestrator task that fans the members out in parallel and
/// injects the synthesis turn. Returns the registered `run_id` (the
/// caller turns it into `{ run_id, event_stream_url }`).
///
/// Errors mirror [`prepare_chat_session`] plus council-specific
/// validation (`council_no_members`, `invalid_council_request`).
pub(crate) async fn start_council_run(
    req: RuntimeRequest,
    ctx: WsContext,
) -> Result<String, ChatRequestError> {
    let (members, conversation_messages) = match req.r#type {
        RuntimeRequestType::Council {
            ref members,
            ref conversation_messages,
        } => (members.clone(), conversation_messages.clone()),
        _ => {
            return Err(ChatRequestError {
                code: "invalid_council_request",
                message: "start_council_run requires a RuntimeRequestType::Council request"
                    .to_string(),
            });
        }
    };

    if members.is_empty() {
        return Err(ChatRequestError {
            code: "council_no_members",
            message: "council run requires at least one member".to_string(),
        });
    }
    let members = truncate_members(members, council_max_members());
    let query = latest_user_query(&conversation_messages);

    // The PARENT run hosts the synthesizer: prepare it with members[0]'s
    // model so the injected synthesis turn runs on the right model.
    let synth_model = members[0].model.clone();
    let parent_model_id = members[0].model.id.clone();
    let registry = ctx.chat_runs.clone();

    let chat_req = RuntimeRequest {
        r#type: RuntimeRequestType::Chat {
            conversation_messages,
        },
        model: synth_model,
        ..req
    };

    let session = prepare_chat_session(chat_req, &ctx).await?;

    // Build the per-session fleet dispatcher BEFORE the session is moved
    // into the chat driver. Members fan out through the identical
    // dispatch surface a `task` tool call uses.
    let fleet_dispatcher = build_fleet_subagent_dispatcher(&session, &ctx).map_err(|e| {
        ChatRequestError {
            code: "council_dispatcher_build_failed",
            message: e.to_string(),
        }
    })?;

    let params = CouncilDispatchParams {
        parent_agent_id: session.agent_id,
        parent_permissions: session.agent_permissions.clone(),
        parent_tool_permissions: session.tool_permissions.clone(),
        user_tool_defaults: ctx
            .store
            .get_user_tool_defaults(&session.user_id)
            .ok()
            .flatten()
            .unwrap_or_default(),
        originating_user_id: Some(session.user_id.clone()),
        parent_model_id,
    };

    let run_id = Uuid::new_v4().to_string();
    // Register + drive the parent run through the shared chat-run path.
    let handle = super::spawn_chat_run(session, ctx, run_id.clone(), registry.clone());
    let shutdown = handle.shutdown.clone();

    info!(
        run_id = %run_id,
        member_count = members.len(),
        "AURA Council run started"
    );

    tokio::spawn(run_council_orchestrator(CouncilOrchestrator {
        handle,
        fleet_dispatcher,
        members,
        query,
        params,
        registry,
        run_id: run_id.clone(),
        shutdown,
    }));

    Ok(run_id)
}

/// Drive a council to completion: wait for the parent session to be
/// ready, fan members out in parallel, then inject the synthesis turn.
async fn run_council_orchestrator(orchestrator: CouncilOrchestrator) {
    let CouncilOrchestrator {
        handle,
        fleet_dispatcher,
        members,
        query,
        params,
        registry,
        run_id,
        shutdown,
    } = orchestrator;

    // Wait for the parent driver's `SessionReady` so the parent identity
    // is registered in the scheduler before members clone it (otherwise
    // members fall back to a bare config and the router buckets them as
    // anonymous traffic). Bounded so a stuck bootstrap never wedges the
    // orchestrator.
    wait_for_session_ready(&handle.events, &shutdown).await;
    if shutdown.is_cancelled() {
        return;
    }

    // A fresh sender into the SAME parent event channel, so the member
    // `SubagentSpawned` / `SubagentStatus` frames land on the parent
    // stream alongside the driver's own frames.
    let parent_outbound = spawn_event_forwarder(handle.events.clone());
    let inner: Arc<dyn EventAwareSubagentDispatch> = fleet_dispatcher;
    let hook = RuntimeSubagentObservabilityHook::new(
        inner,
        parent_outbound,
        registry,
        // Fork member cancellation off the PARENT run's shutdown token so
        // a parent stop / cancel aborts every in-flight member.
        Some(shutdown.clone()),
        Some(run_id.clone()),
    );

    let outcomes = fan_out_members(&hook, &members, &query, &params).await;
    let successes = outcomes.iter().filter(|o| o.is_success()).count();
    info!(
        run_id = %run_id,
        members = members.len(),
        successes,
        "AURA Council fan-out complete"
    );

    // Parent stopped mid-fan-out: skip synthesis; the driver teardown
    // already marked the run done.
    if shutdown.is_cancelled() {
        return;
    }

    let synthesis_prompt = build_synthesis_prompt(&query, &outcomes);
    if handle
        .commands
        .send(InboundMessage::UserMessage(UserMessage {
            content: synthesis_prompt,
            tool_hints: None,
            attachments: None,
        }))
        .await
        .is_err()
    {
        warn!(
            run_id = %run_id,
            "AURA Council: parent run gone before the synthesis turn could start"
        );
    }
}

/// Dispatch every member in PARALLEL via [`futures_util::future::join_all`]
/// (not a sequential loop, not a batch helper). Each member runs the
/// full real-agent loop with its model overridden and `council_index`
/// stamped so the hook labels its `SubagentSpawned`.
async fn fan_out_members(
    hook: &RuntimeSubagentObservabilityHook,
    members: &[CouncilMember],
    query: &str,
    params: &CouncilDispatchParams,
) -> Vec<MemberOutcome> {
    let dispatches = members.iter().enumerate().map(|(idx, member)| {
        let index = u32::try_from(idx).unwrap_or(u32::MAX);
        let request = build_member_request(member, index, query, params);
        let model = member.model.id.clone();
        async move {
            let result = hook.dispatch(request).await;
            MemberOutcome {
                council_index: index,
                model,
                result,
            }
        }
    });
    futures_util::future::join_all(dispatches).await
}

/// Build a member's [`SubagentDispatchRequest`]: the user's query as the
/// prompt, the member's model as `model_override`, parent identity /
/// permissions inherited, and `council_index = Some(index)` so the
/// observability hook stamps the member model + slot onto the emitted
/// `SubagentSpawned`.
fn build_member_request(
    member: &CouncilMember,
    council_index: u32,
    query: &str,
    params: &CouncilDispatchParams,
) -> SubagentDispatchRequest {
    SubagentDispatchRequest {
        parent_agent_id: params.parent_agent_id,
        subagent_type: COUNCIL_MEMBER_KIND.to_string(),
        prompt: query.to_string(),
        originating_user_id: params.originating_user_id.clone(),
        // Members are top-level fan-out runs (no spawning ancestor).
        parent_chain: Vec::new(),
        model_override: member.model.id.clone(),
        system_prompt_addendum: None,
        parent_permissions: params.parent_permissions.clone(),
        parent_tool_permissions: params.parent_tool_permissions.clone(),
        user_tool_defaults: params.user_tool_defaults.clone(),
        tool_call_id: None,
        parent_mode: None,
        parent_kernel_mode: None,
        parent_model_id: params.parent_model_id.clone(),
        override_mode: None,
        override_permissions: None,
        override_tool_subset: None,
        override_isolation_id: None,
        override_budget: None,
        // Wait (block per member, collect the result inline) so the
        // orchestrator can synthesize once every member is terminal.
        spawn_mode: None,
        council_index: Some(council_index),
    }
}

/// Assemble the synthesizer turn's user prompt: the original question
/// plus every member's labelled answer (or a failure note), with an
/// instruction to compare / reconcile and produce one combined answer
/// highlighting agreements + disagreements.
fn build_synthesis_prompt(query: &str, outcomes: &[MemberOutcome]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are the AURA Council synthesizer. Several council member models were each asked the \
         same question in parallel. Compare and reconcile their answers below, then produce ONE \
         combined answer. Explicitly call out where the members AGREE and where they DISAGREE; \
         when they disagree, weigh the trade-offs and state your single best recommendation. Do \
         not merely list the members' answers — integrate them.\n\n",
    );
    prompt.push_str("## Original question\n\n");
    prompt.push_str(query.trim());
    prompt.push_str("\n\n## Council member answers\n\n");
    for outcome in outcomes {
        let model_label = outcome.model.as_deref().unwrap_or("(model unspecified)");
        prompt.push_str(&format!(
            "### Member {} — {}\n\n",
            outcome.council_index, model_label
        ));
        match outcome.member_text() {
            Ok(text) => {
                prompt.push_str(text.trim());
                prompt.push_str("\n\n");
            }
            Err(note) => {
                prompt.push_str(&format!("_(no usable answer — {note})_\n\n"));
            }
        }
    }
    prompt.push_str(
        "## Your synthesized answer\n\nProduce the single combined answer now, beginning with a \
         brief note on where the members agreed and disagreed.",
    );
    prompt
}

/// Resolve the council member cap from `AURA_COUNCIL_MAX_MEMBERS`,
/// falling back to [`DEFAULT_COUNCIL_MAX_MEMBERS`] when unset / invalid /
/// zero.
fn council_max_members() -> usize {
    std::env::var("AURA_COUNCIL_MAX_MEMBERS")
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_COUNCIL_MAX_MEMBERS)
}

/// Silently truncate members beyond `max` (logging a warning). Keeps the
/// first `max` (so `members[0]`, the synthesizer, always survives).
fn truncate_members(mut members: Vec<CouncilMember>, max: usize) -> Vec<CouncilMember> {
    if members.len() > max {
        warn!(
            requested = members.len(),
            max, "AURA Council member count exceeds cap; truncating extras"
        );
        members.truncate(max);
    }
    members
}

/// The user's query a council fans out = the most recent `user` message
/// in the hydrated conversation. Empty when there is none.
fn latest_user_query(messages: &[ConversationMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Poll the parent run's replay history for `SessionReady`, bailing on
/// shutdown or after a bounded number of attempts.
async fn wait_for_session_ready(events: &Arc<ChatEventChannel>, shutdown: &CancellationToken) {
    for _ in 0..200 {
        if shutdown.is_cancelled() {
            return;
        }
        if events
            .subscribe()
            .history
            .iter()
            .any(|m| matches!(m, OutboundMessage::SessionReady(_)))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use aura_agent::AgentLoopEvent;
    use aura_protocol::ModelSelection;
    use dashmap::DashMap;
    use tokio::sync::mpsc;

    /// Test double mirroring `subagent_stream::tests::StubDispatch`,
    /// keyed on the dispatched `council_index` so a single stub can model
    /// per-member success / failure / cancellation.
    struct CouncilStub {
        behavior: StubBehavior,
    }

    enum StubBehavior {
        Completed,
        FailIndex(u32),
        WaitForCancel,
    }

    impl CouncilStub {
        fn completed() -> Self {
            Self {
                behavior: StubBehavior::Completed,
            }
        }
        fn fail_index(index: u32) -> Self {
            Self {
                behavior: StubBehavior::FailIndex(index),
            }
        }
        fn wait_for_cancel() -> Self {
            Self {
                behavior: StubBehavior::WaitForCancel,
            }
        }
    }

    fn stub_result(exit: SubagentExit, final_message: String) -> SubagentResult {
        SubagentResult {
            child_agent_id: None,
            final_message,
            total_input_tokens: 0,
            total_output_tokens: 0,
            files_changed: Vec::new(),
            exit,
        }
    }

    #[async_trait]
    impl EventAwareSubagentDispatch for CouncilStub {
        async fn dispatch_with_events(
            &self,
            request: SubagentDispatchRequest,
            event_tx: Option<mpsc::Sender<AgentLoopEvent>>,
            cancellation: Option<CancellationToken>,
        ) -> Result<SubagentResult, String> {
            let idx = request.council_index.unwrap_or(0);
            match &self.behavior {
                StubBehavior::WaitForCancel => {
                    if let Some(token) = cancellation {
                        token.cancelled().await;
                    }
                    return Ok(stub_result(SubagentExit::Cancelled, String::new()));
                }
                StubBehavior::FailIndex(fail) if *fail == idx => {
                    return Ok(stub_result(
                        SubagentExit::Failed {
                            reason: "member boom".to_string(),
                        },
                        String::new(),
                    ));
                }
                _ => {}
            }
            let text = format!("answer from member {idx}");
            if let Some(tx) = event_tx.as_ref() {
                let _ = tx.send(AgentLoopEvent::TextDelta(text.clone())).await;
            }
            Ok(stub_result(SubagentExit::Completed, text))
        }
    }

    fn test_members(model_ids: &[&str]) -> Vec<CouncilMember> {
        model_ids
            .iter()
            .enumerate()
            .map(|(i, id)| CouncilMember {
                id: i.to_string(),
                model: ModelSelection {
                    id: Some((*id).to_string()),
                    ..ModelSelection::default()
                },
            })
            .collect()
    }

    fn test_params() -> CouncilDispatchParams {
        CouncilDispatchParams {
            parent_agent_id: AgentId::generate(),
            parent_permissions: AgentPermissions::empty(),
            parent_tool_permissions: None,
            user_tool_defaults: UserToolDefaults::full_access(),
            originating_user_id: Some("user-council".to_string()),
            parent_model_id: Some("model-a".to_string()),
        }
    }

    fn hook_with(
        stub: CouncilStub,
        parent_cancellation: CancellationToken,
    ) -> (
        RuntimeSubagentObservabilityHook,
        mpsc::Receiver<OutboundMessage>,
    ) {
        let (parent_tx, parent_rx) = mpsc::channel::<OutboundMessage>(256);
        let registry: ChatRunRegistry = Arc::new(DashMap::new());
        let inner: Arc<dyn EventAwareSubagentDispatch> = Arc::new(stub);
        let hook = RuntimeSubagentObservabilityHook::new(
            inner,
            parent_tx,
            registry,
            Some(parent_cancellation),
            Some("council-run".to_string()),
        );
        (hook, parent_rx)
    }

    /// Fan-out: N members each dispatched in parallel, each emitting a
    /// `SubagentSpawned` carrying the right `model` + `council_index`.
    #[tokio::test]
    async fn fan_out_stamps_model_and_council_index_per_member() {
        let (hook, mut parent_rx) = hook_with(CouncilStub::completed(), CancellationToken::new());
        let members = test_members(&["model-a", "model-b", "model-c"]);
        let params = test_params();

        let outcomes = fan_out_members(&hook, &members, "what is rust?", &params).await;
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes.iter().all(MemberOutcome::is_success));

        let mut spawned: Vec<(Option<u32>, Option<String>)> = Vec::new();
        while let Ok(msg) = parent_rx.try_recv() {
            if let OutboundMessage::SubagentSpawned(s) = msg {
                spawned.push((s.council_index, s.model));
            }
        }
        spawned.sort_by_key(|(index, _)| index.unwrap_or(u32::MAX));
        assert_eq!(
            spawned,
            vec![
                (Some(0), Some("model-a".to_string())),
                (Some(1), Some("model-b".to_string())),
                (Some(2), Some("model-c".to_string())),
            ]
        );
    }

    /// Partial failure: one member fails, the council still synthesizes
    /// from the rest and notes the failure.
    #[tokio::test]
    async fn partial_failure_still_synthesizes_from_survivors() {
        let (hook, _parent_rx) = hook_with(CouncilStub::fail_index(1), CancellationToken::new());
        let members = test_members(&["m0", "m1", "m2"]);
        let params = test_params();

        let outcomes = fan_out_members(&hook, &members, "q", &params).await;
        assert!(outcomes[0].is_success());
        assert!(!outcomes[1].is_success(), "member 1 failed");
        assert!(outcomes[2].is_success());

        let prompt = build_synthesis_prompt("q", &outcomes);
        assert!(prompt.contains("Member 0 — m0"));
        assert!(prompt.contains("answer from member 0"));
        assert!(prompt.contains("Member 1 — m1"));
        assert!(prompt.contains("no usable answer"));
        assert!(prompt.contains("answer from member 2"));
    }

    /// Cancel: cancelling the PARENT token aborts every in-flight member
    /// (the hook forks each member's token off it).
    #[tokio::test]
    async fn parent_cancellation_aborts_in_flight_members() {
        let parent_cancel = CancellationToken::new();
        let (hook, _parent_rx) = hook_with(CouncilStub::wait_for_cancel(), parent_cancel.clone());
        let members = test_members(&["m0", "m1"]);
        let params = test_params();

        let fan = tokio::spawn(async move {
            fan_out_members(&hook, &members, "q", &params).await
        });

        // Let both dispatches reach their cancellation await.
        tokio::time::sleep(Duration::from_millis(50)).await;
        parent_cancel.cancel();

        let outcomes = fan.await.expect("fan-out task joins");
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes.iter().all(|o| matches!(
            &o.result,
            Ok(result) if result.exit == SubagentExit::Cancelled
        )));
    }

    /// Cap: more than the cap is truncated, keeping the first `max`
    /// (so the synthesizer slot 0 always survives).
    #[test]
    fn truncate_members_caps_and_keeps_synthesizer() {
        let members = test_members(&["a", "b", "c", "d", "e", "f"]);
        let capped = truncate_members(members, 4);
        let ids: Vec<String> = capped
            .iter()
            .map(|m| m.model.id.clone().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn truncate_members_keeps_all_under_cap() {
        let members = test_members(&["a", "b"]);
        assert_eq!(truncate_members(members, 4).len(), 2);
    }

    #[test]
    fn latest_user_query_returns_most_recent_user_message() {
        let messages = vec![
            ConversationMessage {
                role: "user".to_string(),
                content: "first".to_string(),
            },
            ConversationMessage {
                role: "assistant".to_string(),
                content: "reply".to_string(),
            },
            ConversationMessage {
                role: "user".to_string(),
                content: "latest question".to_string(),
            },
        ];
        assert_eq!(latest_user_query(&messages), "latest question");
        assert_eq!(latest_user_query(&[]), "");
    }
}
