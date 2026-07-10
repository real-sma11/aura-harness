//! AURA Council orchestrator.
//!
//! A council run convenes `members` model seats to answer the user's
//! question, then synthesizes one combined response.
//!
//! The fan-out is **deterministic and harness-orchestrated** rather than
//! model-driven: instead of prompting the synthesizer model to "please
//! call the `task` tool N times" (which is non-deterministic, slow, and
//! emits ordinary task spawns the UI can't group), the coordinator
//! dispatches the N member child runs DIRECTLY through the same
//! [`super::subagent_stream::RuntimeSubagentObservabilityHook`] +
//! [`aura_fleet_subagent::FleetSubagentDispatcher`] the `task` tool uses.
//! That reuses all the existing, already-working subagent machinery
//! (child-run registration, live WS-attachable threads, status frames)
//! while giving the council exactly the shape the UI expects:
//!
//! - The PARENT run is created + registered through the SAME
//!   [`super::chat_run::spawn_chat_run`] path a `POST /v1/run` chat run
//!   uses (so `WS /stream/:run_id` attaches non-destructively), prepared
//!   with `members[0]`'s model — the synthesizer.
//! - Once the parent session is ready, the coordinator dispatches every
//!   member as a council-tagged subagent (`council_index = Some(i)`,
//!   `model_override = member.model`, prompt = the user's question)
//!   IN PARALLEL. Each emits a `SubagentSpawned { council_index, model,
//!   parent_tool_use_id }` on the parent stream IMMEDIATELY — so all N
//!   member columns appear at once — and streams live on its own child
//!   run. All members share one synthetic `council_parent_tool_use_id`
//!   so the UI folds them into a single council panel (N columns), while
//!   each still dispatches with its own `tool_call_id` so the
//!   per-`(parent, tool_call_id)` dedupe never collapses two members.
//! - When every member returns, the coordinator injects ONE synthesis
//!   `user_message` carrying the members' answers; the synthesizer's
//!   normal text turn is the combined answer rendered below the panel.
//!
//! Cancellation: each member's child token is forked from the parent
//! run's `shutdown` token, so a single `POST /v1/run/:id/stop` (or a
//! parent `Cancel`) aborts every in-flight member; the coordinator also
//! bails before injecting synthesis if `shutdown` fired.

use std::sync::Arc;
use std::time::Duration;

use aura_core_types::{
    AgentId, AgentPermissions, AgentToolPermissions, SubagentDispatchRequest, SubagentExit,
    SubagentResult, UserToolDefaults,
};
use aura_fleet_subagent::FleetSubagentDispatcher;
use aura_protocol::{
    ConversationMessage, CouncilMechanism, CouncilMember, CouncilMemberRole, RuntimeRequest,
    RuntimeRequestType,
};
use aura_tools::SubagentDispatchHook;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use super::chat_run::{spawn_event_forwarder, ChatEventChannel, ChatRunHandle, ChatRunRegistry};
use super::helpers::{build_fleet_subagent_dispatcher, prepare_chat_session, ChatRequestError};
use super::subagent_stream::RuntimeSubagentObservabilityHook;
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

/// Hermes-style advisor contract for Second Opinion references. This is
/// carried as a child system-prompt addendum and paired with an empty
/// `override_tool_subset`, so the runtime policy and prompt agree: the
/// reference model advises only; it is not the acting/final agent.
const SECOND_OPINION_REFERENCE_ADVISOR_PROMPT: &str = "\
Second Opinion advisor contract. This addendum overrides any generic subagent wording above.\n\n\
You are a reference advisor in Aura Second Opinion. You are NOT the acting/final agent and you do \
NOT execute anything: no tools are available to you, and you must not claim to have read files, run \
commands, browsed, edited code, or inspected external state unless that evidence is explicitly \
included in the prompt. Do not apologize for lack of access.\n\n\
Give private guidance for the final model. Base every claim on the provided conversation and user \
request. Separate known facts from assumptions. Flag unsupported claims, missing context, likely \
risks, disagreements, and concrete next steps. Return advice only; do not answer as if you are the \
user-facing assistant.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CouncilRunKind {
    Council,
    SecondOpinion,
}

/// Parent-derived inputs the coordinator needs to build each member's
/// [`SubagentDispatchRequest`]. Snapshotted from the prepared
/// [`super::Session`] BEFORE it is moved into the parent run driver
/// (mirrors the fields the `task` tool pulls off its `ToolContext`).
struct CouncilDispatchParams {
    parent_agent_id: AgentId,
    parent_permissions: AgentPermissions,
    parent_tool_permissions: Option<AgentToolPermissions>,
    user_tool_defaults: UserToolDefaults,
    originating_user_id: String,
    parent_model_id: String,
    chat_runs: ChatRunRegistry,
    dispatcher: Arc<FleetSubagentDispatcher>,
    /// Combine mechanism stamped onto every member's `SubagentSpawned`
    /// so the UI can label the council panel with the active mechanism.
    mechanism: CouncilMechanism,
}

/// Everything the detached coordinator task needs to fan a council out
/// once the parent run is registered + ready.
struct CouncilCoordinator {
    handle: Arc<ChatRunHandle>,
    members: Vec<CouncilMember>,
    mechanism: CouncilMechanism,
    run_kind: CouncilRunKind,
    query: String,
    run_id: String,
    shutdown: CancellationToken,
    params: CouncilDispatchParams,
}

/// Start an AURA Council run.
///
/// Mirrors [`super::chat_run::spawn_chat_run`]'s setup to create +
/// register the PARENT run (hosting the synthesizer, `members[0]`), then
/// detaches a coordinator task that — once the session is ready —
/// directly dispatches the members as council-tagged subagents and, when
/// they return, injects the synthesis turn. Returns the registered
/// `run_id` (the caller turns it into `{ run_id, event_stream_url }`).
///
/// Errors mirror [`prepare_chat_session`] plus council-specific
/// validation (`council_no_members`, `invalid_council_request`).
pub(crate) async fn start_council_run(
    req: RuntimeRequest,
    ctx: WsContext,
) -> Result<String, ChatRequestError> {
    let (members, mechanism, conversation_messages) = match req.r#type {
        RuntimeRequestType::Council {
            ref members,
            mechanism,
            ref conversation_messages,
        } => (members.clone(), mechanism, conversation_messages.clone()),
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
    let run_kind = classify_council_members(&members)?;
    let query = latest_user_query(&conversation_messages);

    // The PARENT run hosts the synthesizer/final model: prepare it with
    // members[0]'s model so the synthesis/final turn runs on the first
    // model. For Second Opinion, slot 0 is validated as the aggregator
    // and reference slots are advisory-only children.
    let synth_model = members[0].model.clone();
    let registry = ctx.chat_runs.clone();

    let chat_req = RuntimeRequest {
        r#type: RuntimeRequestType::Chat {
            conversation_messages,
        },
        model: synth_model,
        ..req
    };

    let session = prepare_chat_session(chat_req, &ctx).await?;

    // Build the member-dispatch surface and snapshot the parent identity
    // / permissions BEFORE `session` + `ctx` are moved into the driver.
    // The dispatcher is the SAME per-session fleet dispatcher the
    // `task`-tool path constructs, so members inherit the parent's
    // identity / permissions / workspace and run the full real-agent
    // loop with only their model overridden.
    let dispatcher =
        build_fleet_subagent_dispatcher(&session, &ctx).map_err(|e| ChatRequestError {
            code: "council_dispatcher_build_failed",
            message: format!("failed to build council member dispatcher: {e}"),
        })?;
    let user_tool_defaults =
        super::helpers::session_user_defaults(&session, &ctx).map_err(|e| ChatRequestError {
            code: "council_user_defaults_failed",
            message: format!("failed to resolve council user tool defaults: {e}"),
        })?;
    let params = CouncilDispatchParams {
        parent_agent_id: session.agent_id,
        parent_permissions: session.agent_permissions.clone(),
        parent_tool_permissions: session.tool_permissions.clone(),
        user_tool_defaults,
        originating_user_id: session.user_id.clone(),
        parent_model_id: session.model.clone(),
        chat_runs: registry.clone(),
        dispatcher,
        mechanism,
    };

    let run_id = Uuid::new_v4().to_string();
    let handle = super::spawn_chat_run(session, ctx, run_id.clone(), registry);
    let shutdown = handle.shutdown.clone();

    info!(
        run_id = %run_id,
        member_count = members.len(),
        mechanism = mechanism.as_wire(),
        run_kind = ?run_kind,
        "AURA Council run started"
    );

    tokio::spawn(run_council_coordinator(CouncilCoordinator {
        handle,
        members,
        mechanism,
        run_kind,
        query,
        run_id: run_id.clone(),
        shutdown,
        params,
    }));

    Ok(run_id)
}

/// Drive a council: wait for the parent session to be ready, dispatch
/// every member in parallel as a council-tagged subagent, then inject a
/// synthesis turn built from their answers.
async fn run_council_coordinator(coordinator: CouncilCoordinator) {
    let CouncilCoordinator {
        handle,
        members,
        mechanism,
        run_kind,
        query,
        run_id,
        shutdown,
        params,
    } = coordinator;

    // Wait for the parent driver's `SessionReady` before dispatching so
    // the parent identity is registered in the scheduler before members
    // spawn off it (otherwise they fall back to a bare config and the
    // router buckets them as anonymous traffic). Bounded so a stuck
    // bootstrap never wedges the coordinator.
    wait_for_session_ready(&handle.events, &shutdown).await;
    if shutdown.is_cancelled() {
        return;
    }

    let answers = dispatch_members(
        &handle, &members, run_kind, &query, &run_id, &shutdown, &params,
    )
    .await;
    if shutdown.is_cancelled() {
        return;
    }

    // Inject the final turn. The parent model's normal text turn is the
    // combined/final answer the UI renders below the panel.
    let prompt = match run_kind {
        CouncilRunKind::Council => build_synthesis_prompt(&query, &answers, mechanism),
        CouncilRunKind::SecondOpinion => build_second_opinion_prompt(&query, &answers),
    };
    if handle
        .commands
        .send(InboundMessage::UserMessage(UserMessage {
            content: prompt,
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

/// One council member's resolved outcome, carried into the synthesis
/// prompt in `council_index` order.
struct MemberAnswer {
    index: usize,
    model_label: String,
    outcome: Result<SubagentResult, String>,
}

/// Dispatch every council member in parallel through the shared
/// observability hook. Each member emits a `SubagentSpawned` (with
/// `council_index`, `model`, and the shared `council_parent_tool_use_id`)
/// on the parent stream immediately, streams live on its own child run,
/// and resolves to a [`SubagentResult`]. Returns the answers in
/// `council_index` order for synthesis.
async fn dispatch_members(
    handle: &Arc<ChatRunHandle>,
    members: &[CouncilMember],
    run_kind: CouncilRunKind,
    query: &str,
    run_id: &str,
    shutdown: &CancellationToken,
    params: &CouncilDispatchParams,
) -> Vec<MemberAnswer> {
    // Emit member spawn/status frames into the SAME replay channel the
    // parent run streams over, via a forwarder onto its event channel.
    let parent_outbound = spawn_event_forwarder(handle.events.clone());
    let hook = Arc::new(
        RuntimeSubagentObservabilityHook::new(
            params.dispatcher.clone(),
            parent_outbound,
            params.chat_runs.clone(),
            Some(shutdown.clone()),
            Some(run_id.to_string()),
        )
        .with_council_mechanism(Some(params.mechanism.as_wire().to_string())),
    );

    // One synthetic grouping id every member shares so the UI folds them
    // into a single council panel (N columns). Distinct from each
    // member's `tool_call_id` (left `None`) so the `(parent,
    // tool_call_id)` dedupe never collapses two members into one child.
    let group_id = format!("council-{run_id}");

    info!(
        run_id = %run_id,
        member_count = members.len(),
        run_kind = ?run_kind,
        "AURA Council: dispatching members"
    );

    let dispatches = members
        .iter()
        .enumerate()
        .filter(|(index, member)| should_dispatch_member(run_kind, *index, member))
        .map(|(index, member)| {
            let hook = hook.clone();
            let model_label = member.model.id.clone().unwrap_or_default();
            let request = SubagentDispatchRequest {
                parent_agent_id: params.parent_agent_id,
                subagent_type: COUNCIL_MEMBER_KIND.to_string(),
                prompt: member_prompt(run_kind, query),
                originating_user_id: Some(params.originating_user_id.clone()),
                // Members hang directly off the parent (depth 1).
                parent_chain: vec![params.parent_agent_id],
                model_override: member.model.id.clone(),
                system_prompt_addendum: member_system_prompt_addendum(run_kind),
                parent_permissions: params.parent_permissions.clone(),
                parent_tool_permissions: params.parent_tool_permissions.clone(),
                user_tool_defaults: params.user_tool_defaults.clone(),
                // No dedupe key: each member is a distinct dispatch.
                tool_call_id: None,
                parent_mode: None,
                parent_kernel_mode: None,
                parent_model_id: Some(params.parent_model_id.clone()),
                override_mode: None,
                override_permissions: None,
                override_tool_subset: member_tool_subset(run_kind),
                override_isolation_id: None,
                override_budget: None,
                // Wait: block until the member finishes; the hook still emits
                // the spawn frame up-front so all columns appear immediately.
                spawn_mode: None,
                council_index: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                council_parent_tool_use_id: Some(group_id.clone()),
            };
            async move {
                let outcome = hook.dispatch(request).await;
                MemberAnswer {
                    index,
                    model_label,
                    outcome,
                }
            }
        });

    let mut answers = futures_util::future::join_all(dispatches).await;
    answers.sort_by_key(|a| a.index);
    answers
}

fn classify_council_members(members: &[CouncilMember]) -> Result<CouncilRunKind, ChatRequestError> {
    let role_count = members.iter().filter(|m| m.role.is_some()).count();
    if role_count == 0 {
        return Ok(CouncilRunKind::Council);
    }
    if members.first().and_then(|m| m.role) != Some(CouncilMemberRole::Aggregator) {
        return Err(ChatRequestError {
            code: "invalid_second_opinion_request",
            message: "Second Opinion requires member 0 to have role `aggregator`".to_string(),
        });
    }
    if members.len() < 2 {
        return Err(ChatRequestError {
            code: "second_opinion_no_references",
            message: "Second Opinion requires at least one reference member".to_string(),
        });
    }
    for (index, member) in members.iter().enumerate().skip(1) {
        if member.role != Some(CouncilMemberRole::Reference) {
            return Err(ChatRequestError {
                code: "invalid_second_opinion_request",
                message: format!("Second Opinion member {index} must have role `reference`"),
            });
        }
    }
    Ok(CouncilRunKind::SecondOpinion)
}

fn should_dispatch_member(run_kind: CouncilRunKind, index: usize, member: &CouncilMember) -> bool {
    match run_kind {
        CouncilRunKind::Council => true,
        CouncilRunKind::SecondOpinion => {
            index > 0 && member.role == Some(CouncilMemberRole::Reference)
        }
    }
}

fn member_prompt(run_kind: CouncilRunKind, query: &str) -> String {
    match run_kind {
        CouncilRunKind::Council => query.to_string(),
        CouncilRunKind::SecondOpinion => build_reference_advisor_prompt(query),
    }
}

fn member_system_prompt_addendum(run_kind: CouncilRunKind) -> Option<String> {
    match run_kind {
        CouncilRunKind::Council => None,
        CouncilRunKind::SecondOpinion => Some(SECOND_OPINION_REFERENCE_ADVISOR_PROMPT.to_string()),
    }
}

fn member_tool_subset(run_kind: CouncilRunKind) -> Option<Vec<String>> {
    match run_kind {
        CouncilRunKind::Council => None,
        CouncilRunKind::SecondOpinion => Some(Vec::new()),
    }
}

fn build_reference_advisor_prompt(query: &str) -> String {
    format!(
        "\
## Second Opinion reference task\n\n\
You are reviewing the user's request for the final Aura model. Give private advice only.\n\n\
## User request\n\n\
{}\n\n\
## Factual rubric\n\n\
- Ground claims in the request above.\n\
- Mark assumptions explicitly.\n\
- Flag missing evidence and likely risks.\n\
- Recommend concrete next steps for the final model.\n\
- Do not claim to have used tools or inspected external state.",
        query.trim()
    )
}

/// Build the final combine turn: embed the user's question and each
/// member's answer (or failure), then direct `members[0]` to combine
/// them according to the chosen [`CouncilMechanism`].
fn build_synthesis_prompt(
    query: &str,
    answers: &[MemberAnswer],
    mechanism: CouncilMechanism,
) -> String {
    let n = answers.len();
    let mut prompt = String::new();
    prompt.push_str(&format!(
        "You are the AURA Council synthesizer. {n} council member model(s) independently answered \
         the user's question. {}\n\n",
        mechanism_intro(mechanism)
    ));

    prompt.push_str("## User question\n\n");
    prompt.push_str(query.trim());

    prompt.push_str("\n\n## Council member answers\n");
    for answer in answers {
        let label = if answer.model_label.is_empty() {
            "(default model)".to_string()
        } else {
            answer.model_label.clone()
        };
        prompt.push_str(&format!("\n### Member {} — `{label}`\n\n", answer.index));
        match &answer.outcome {
            Ok(result) => match &result.exit {
                SubagentExit::Completed => {
                    let body = result.final_message.trim();
                    if body.is_empty() {
                        prompt.push_str("(member returned an empty answer)\n");
                    } else {
                        prompt.push_str(body);
                        prompt.push('\n');
                    }
                }
                SubagentExit::Failed { reason } => {
                    prompt.push_str(&format!("(member failed: {reason})\n"));
                }
                SubagentExit::Cancelled => prompt.push_str("(member was cancelled)\n"),
                SubagentExit::Timeout => prompt.push_str("(member timed out)\n"),
                SubagentExit::Rejected { reason } => {
                    prompt.push_str(&format!("(member rejected: {reason})\n"));
                }
            },
            Err(err) => {
                prompt.push_str(&format!("(member dispatch error: {err})\n"));
            }
        }
    }

    prompt.push_str(mechanism_instruction(mechanism));
    prompt
}

/// Build the final Second Opinion turn. Reference outputs are private
/// guidance, not authoritative facts. Unlike the ordinary Council
/// synthesizer prompt, this does not forbid tool use; the final model
/// remains the acting agent and can verify facts when the request calls
/// for it.
fn build_second_opinion_prompt(query: &str, answers: &[MemberAnswer]) -> String {
    let mut prompt = String::new();
    prompt.push_str(
        "You are the final model in Aura Second Opinion. Reference advisor model(s) reviewed the \
         same user request and produced private guidance below.\n\n\
         Treat reference guidance as advisory, not as ground truth. Your answer must be based on \
         facts present in the conversation, tool results you actually inspect, or explicit user \
         input. If references conflict or make unsupported claims, resolve the conflict using \
         evidence or say what is uncertain. You may call tools if verification or task completion \
         requires it; otherwise answer directly.\n\n",
    );

    prompt.push_str("## User request\n\n");
    prompt.push_str(query.trim());

    prompt.push_str("\n\n## Reference advisor guidance\n");
    for answer in answers {
        let label = if answer.model_label.is_empty() {
            "(default model)".to_string()
        } else {
            answer.model_label.clone()
        };
        prompt.push_str(&format!("\n### Reference {} — `{label}`\n\n", answer.index));
        append_member_outcome(&mut prompt, answer, "reference");
    }

    prompt.push_str(
        "\n## Final response rubric\n\n\
         Write the user-facing answer. Use the references to improve coverage and catch mistakes, \
         but do not cite reference claims as facts unless they are supported by the visible \
         conversation or verified evidence. Be explicit about uncertainty and concrete next steps.",
    );
    prompt
}

fn append_member_outcome(prompt: &mut String, answer: &MemberAnswer, label: &str) {
    match &answer.outcome {
        Ok(result) => match &result.exit {
            SubagentExit::Completed => {
                let body = result.final_message.trim();
                if body.is_empty() {
                    prompt.push_str(&format!("({label} returned an empty answer)\n"));
                } else {
                    prompt.push_str(body);
                    prompt.push('\n');
                }
            }
            SubagentExit::Failed { reason } => {
                prompt.push_str(&format!("({label} failed: {reason})\n"));
            }
            SubagentExit::Cancelled => prompt.push_str(&format!("({label} was cancelled)\n")),
            SubagentExit::Timeout => prompt.push_str(&format!("({label} timed out)\n")),
            SubagentExit::Rejected { reason } => {
                prompt.push_str(&format!("({label} rejected: {reason})\n"));
            }
        },
        Err(err) => {
            prompt.push_str(&format!("({label} dispatch error: {err})\n"));
        }
    }
}

/// One-line framing for the chosen mechanism, dropped into the prompt
/// preamble so the synthesizer knows its job before reading the answers.
fn mechanism_intro(mechanism: CouncilMechanism) -> &'static str {
    match mechanism {
        CouncilMechanism::Synthesize => "Integrate their answers into ONE combined response.",
        CouncilMechanism::Contrast => {
            "Compare and contrast their answers, surfacing agreements and disagreements."
        }
        CouncilMechanism::SideBySide => {
            "Present each member's answer side by side, faithfully and without merging them."
        }
    }
}

/// The closing instruction block that tells `members[0]` how to combine
/// the members' answers for the chosen mechanism. Every variant forbids
/// tool calls so the combine turn is a single direct text response the
/// UI renders below the council panel.
fn mechanism_instruction(mechanism: CouncilMechanism) -> &'static str {
    match mechanism {
        CouncilMechanism::Synthesize => {
            "\n## Synthesize\n\n\
             Write ONE combined answer. Explicitly call out where the members AGREE and where they \
             DISAGREE; when they disagree, weigh the trade-offs and state your single best \
             recommendation. Integrate their answers — do not merely list them. Do NOT call any \
             tools; respond with the synthesized answer directly."
        }
        CouncilMechanism::Contrast => {
            "\n## Contrast\n\n\
             Write a structured comparison of the members' answers. Use a section (or table) for \
             the points where they AGREE and a section for where they DISAGREE, attributing each \
             position to the member(s) that hold it. Highlight the key trade-offs that distinguish \
             the answers. Do NOT collapse everything into a single recommendation — the goal is to \
             make the differences clear so the reader can decide. Do NOT call any tools; respond \
             with the contrast directly."
        }
        CouncilMechanism::SideBySide => {
            "\n## Side by side\n\n\
             Present each member's answer side by side under its own clearly labeled heading \
             (`## Member <index> — <model>`), reproducing each answer faithfully with only light \
             cleanup for readability. Do NOT merge, rank, editorialize, or add a recommendation — \
             keep every member's response distinct and self-contained. Do NOT call any tools; \
             respond with the side-by-side presentation directly."
        }
    }
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
    use aura_protocol::ModelSelection;

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
                role: None,
            })
            .collect()
    }

    fn second_opinion_members(model_ids: &[&str]) -> Vec<CouncilMember> {
        model_ids
            .iter()
            .enumerate()
            .map(|(i, id)| CouncilMember {
                id: i.to_string(),
                model: ModelSelection {
                    id: Some((*id).to_string()),
                    ..ModelSelection::default()
                },
                role: Some(if i == 0 {
                    CouncilMemberRole::Aggregator
                } else {
                    CouncilMemberRole::Reference
                }),
            })
            .collect()
    }

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

    #[test]
    fn classify_members_detects_second_opinion_only_with_complete_roles() {
        let council = test_members(&["final", "ref"]);
        assert_eq!(
            classify_council_members(&council).expect("plain council"),
            CouncilRunKind::Council
        );

        let second_opinion = second_opinion_members(&["final", "ref-a", "ref-b"]);
        assert_eq!(
            classify_council_members(&second_opinion).expect("second opinion"),
            CouncilRunKind::SecondOpinion
        );

        let mut missing_reference_role = second_opinion_members(&["final", "ref"]);
        missing_reference_role[1].role = None;
        let err = classify_council_members(&missing_reference_role)
            .expect_err("partial roles must fail closed");
        assert_eq!(err.code, "invalid_second_opinion_request");

        let aggregator_only = vec![CouncilMember {
            id: "0".into(),
            model: ModelSelection {
                id: Some("final".into()),
                ..ModelSelection::default()
            },
            role: Some(CouncilMemberRole::Aggregator),
        }];
        let err = classify_council_members(&aggregator_only).expect_err("references are required");
        assert_eq!(err.code, "second_opinion_no_references");
    }

    #[test]
    fn second_opinion_dispatches_references_only_with_no_tools() {
        let members = second_opinion_members(&["final", "kimi", "deepseek"]);
        let dispatched: Vec<usize> = members
            .iter()
            .enumerate()
            .filter(|(index, member)| {
                should_dispatch_member(CouncilRunKind::SecondOpinion, *index, member)
            })
            .map(|(index, _)| index)
            .collect();

        assert_eq!(
            dispatched,
            vec![1, 2],
            "aggregator slot 0 must not run as a reference child"
        );
        assert_eq!(
            member_tool_subset(CouncilRunKind::SecondOpinion),
            Some(vec![])
        );
        let addendum =
            member_system_prompt_addendum(CouncilRunKind::SecondOpinion).expect("advisor addendum");
        assert!(addendum.contains("NOT the acting/final agent"));
        assert!(addendum.contains("no tools are available"));
        assert!(addendum.contains("Separate known facts from assumptions"));
    }

    #[test]
    fn second_opinion_reference_prompt_uses_factual_rubric() {
        let prompt = member_prompt(CouncilRunKind::SecondOpinion, "review this migration");

        assert!(prompt.contains("Second Opinion reference task"));
        assert!(prompt.contains("review this migration"));
        assert!(prompt.contains("Ground claims in the request above"));
        assert!(prompt.contains("Mark assumptions explicitly"));
        assert!(prompt.contains("Do not claim to have used tools"));
    }

    fn completed_answer(index: usize, model: &str, body: &str) -> MemberAnswer {
        MemberAnswer {
            index,
            model_label: model.to_string(),
            outcome: Ok(SubagentResult {
                child_agent_id: None,
                final_message: body.to_string(),
                total_input_tokens: 0,
                total_output_tokens: 0,
                files_changed: Vec::new(),
                exit: SubagentExit::Completed,
            }),
        }
    }

    /// The synthesis prompt must embed the question, each member's model
    /// label + answer, and steer the synthesizer to integrate (not list)
    /// them.
    #[test]
    fn synthesis_prompt_embeds_question_and_member_answers() {
        let answers = vec![
            completed_answer(0, "model-a", "rust is a systems language"),
            completed_answer(1, "model-b", "rust has a borrow checker"),
        ];
        let prompt =
            build_synthesis_prompt("what is rust?", &answers, CouncilMechanism::Synthesize);

        assert!(prompt.contains("what is rust?"), "embeds the question");
        assert!(
            prompt.contains("model-a") && prompt.contains("model-b"),
            "labels members"
        );
        assert!(
            prompt.contains("rust is a systems language")
                && prompt.contains("rust has a borrow checker"),
            "embeds each member's answer"
        );
        assert!(prompt.contains("Member 0") && prompt.contains("Member 1"));
        assert!(
            prompt.to_lowercase().contains("synthesize")
                || prompt.to_lowercase().contains("combined"),
            "asks for synthesis"
        );
    }

    /// A failed / rejected member is surfaced (with its reason) rather
    /// than silently dropped, so the synthesizer can account for it.
    #[test]
    fn synthesis_prompt_surfaces_member_failures() {
        let answers = vec![
            completed_answer(0, "model-a", "ok answer"),
            MemberAnswer {
                index: 1,
                model_label: "model-b".to_string(),
                outcome: Ok(SubagentResult::rejected("depth exceeded")),
            },
        ];
        let prompt = build_synthesis_prompt("q", &answers, CouncilMechanism::Synthesize);
        assert!(prompt.contains("ok answer"));
        assert!(
            prompt.contains("depth exceeded"),
            "rejected member reason is surfaced: {prompt}"
        );
    }

    #[test]
    fn second_opinion_final_prompt_treats_references_as_private_guidance() {
        let answers = vec![
            completed_answer(1, "kimi", "Likely issue is stale cache."),
            MemberAnswer {
                index: 2,
                model_label: "deepseek".into(),
                outcome: Err("network timeout".into()),
            },
        ];
        let prompt = build_second_opinion_prompt("Why is CI failing?", &answers);

        assert!(prompt.contains("final model in Aura Second Opinion"));
        assert!(prompt.contains("private guidance"));
        assert!(prompt.contains("advisory, not as ground truth"));
        assert!(prompt.contains("facts present in the conversation"));
        assert!(prompt.contains("You may call tools if verification"));
        assert!(prompt.contains("Likely issue is stale cache."));
        assert!(prompt.contains("reference dispatch error: network timeout"));
        assert!(
            !prompt.contains("Do NOT call any tools"),
            "Second Opinion final model should retain acting-agent ability to verify"
        );
    }

    /// Each mechanism steers the combine turn with its own instruction
    /// block while still embedding every member's answer.
    #[test]
    fn synthesis_prompt_varies_by_mechanism() {
        let answers = vec![
            completed_answer(0, "model-a", "answer one"),
            completed_answer(1, "model-b", "answer two"),
        ];

        let synthesize = build_synthesis_prompt("q", &answers, CouncilMechanism::Synthesize);
        assert!(synthesize.contains("## Synthesize"));
        assert!(synthesize.contains("ONE combined answer"));

        let contrast = build_synthesis_prompt("q", &answers, CouncilMechanism::Contrast);
        assert!(contrast.contains("## Contrast"));
        assert!(
            contrast.contains("Do NOT collapse everything into a single recommendation"),
            "contrast must not force a single recommendation: {contrast}"
        );

        let side_by_side = build_synthesis_prompt("q", &answers, CouncilMechanism::SideBySide);
        assert!(side_by_side.contains("## Side by side"));
        assert!(
            side_by_side.contains("Do NOT merge"),
            "side-by-side must not merge answers: {side_by_side}"
        );

        // Every mechanism still embeds each member's answer.
        for prompt in [&synthesize, &contrast, &side_by_side] {
            assert!(prompt.contains("answer one") && prompt.contains("answer two"));
        }
    }
}
