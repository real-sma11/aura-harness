//! Chat automaton – handles a single chat round-trip.
//!
//! Replaces `ChatService::send_message_streaming()` from `aura-app`.
//! This is an on-demand automaton: each tick runs one chat interaction
//! (system prompt + messages → agent loop → result) then returns `Done`.

use std::sync::Arc;

use tracing::{error, info};

use aura_agent::agent_runner::{AgentRunner, AgentRunnerConfig};
use aura_prompts::ProjectInfo;
use aura_reasoner::{Message, ModelProvider};
use aura_tools::catalog::{ToolCatalog, ToolProfile};
use aura_tools::domain_tools::{DomainApi, MessageDescriptor, SaveMessageParams};

use super::noop_executor::NoOpExecutor;
use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;
use crate::runtime::{Automaton, TickOutcome};
use crate::schedule::Schedule;

pub struct ChatAutomaton {
    domain: Arc<dyn DomainApi>,
    provider: Arc<dyn ModelProvider>,
    runner: AgentRunner,
    catalog: Arc<ToolCatalog>,
}

impl ChatAutomaton {
    /// Construct a chat automaton bound to a kernel-mediated model
    /// provider.
    ///
    /// The `RecordingModelProvider` bound (sealed in `aura-agent`,
    /// Invariant §1 / §3) means external crates can only satisfy this by
    /// passing an `Arc<aura_agent::KernelModelGateway>`, so a raw HTTP
    /// `ModelProvider` cannot reach the chat automaton without first
    /// flowing through `Kernel::reason_streaming`.
    pub fn new<P>(
        domain: Arc<dyn DomainApi>,
        provider: Arc<P>,
        config: AgentRunnerConfig,
        catalog: Arc<ToolCatalog>,
    ) -> Self
    where
        P: aura_agent::RecordingModelProvider + Send + Sync + 'static,
    {
        let provider: Arc<dyn ModelProvider> = provider;
        Self {
            domain,
            provider,
            runner: AgentRunner::new(config),
            catalog,
        }
    }
}

struct ChatConfig {
    project_id: String,
    instance_id: String,
    custom_system_prompt: String,
}

impl ChatConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, AutomatonError> {
        let project_id = config
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing project_id".into()))?
            .to_string();
        let instance_id = config
            .get("agent_instance_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let custom_system_prompt = config
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(Self {
            project_id,
            instance_id,
            custom_system_prompt,
        })
    }
}

#[async_trait::async_trait]
impl Automaton for ChatAutomaton {
    fn kind(&self) -> &'static str {
        "chat"
    }

    fn default_schedule(&self) -> Schedule {
        Schedule::OnDemand
    }

    async fn tick(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        let cfg = ChatConfig::from_json(&ctx.config)?;

        let stored = self.load_messages(ctx, &cfg).await?;
        let (project_info, api_messages, tools) =
            self.build_chat_context(ctx, &cfg, &stored).await?;

        let result = self
            .run_chat_loop(ctx, &project_info, &cfg, api_messages, tools)
            .await?;
        self.save_assistant_reply(ctx, &cfg, &result).await?;

        ctx.emit(AutomatonEvent::TokenUsage {
            task_id: None,
            input_tokens: result.total_input_tokens,
            output_tokens: result.total_output_tokens,
        })?;

        Ok(TickOutcome::Done)
    }
}

impl ChatAutomaton {
    async fn load_messages(
        &self,
        ctx: &mut TickContext,
        cfg: &ChatConfig,
    ) -> Result<Vec<MessageDescriptor>, AutomatonError> {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Loading conversation...".into(),
        })?;

        let stored = self
            .domain
            .list_messages(&cfg.project_id, &cfg.instance_id)
            .await
            .map_err(|e| AutomatonError::domain_api(None, e.context("list_messages")))?;

        if stored.is_empty() {
            ctx.emit(AutomatonEvent::Error {
                automaton_id: ctx.automaton_id.to_string(),
                message: "No messages to process".into(),
            })?;
            return Err(AutomatonError::domain_api(
                None,
                anyhow::anyhow!("No messages to process"),
            ));
        }

        Ok(stored)
    }

    async fn build_chat_context(
        &self,
        ctx: &mut TickContext,
        cfg: &ChatConfig,
        stored: &[MessageDescriptor],
    ) -> Result<
        (
            aura_tools::domain_tools::ProjectDescriptor,
            Vec<Message>,
            Vec<aura_reasoner::ToolDefinition>,
        ),
        AutomatonError,
    > {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Building context...".into(),
        })?;

        let project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(None, e))?;

        let api_messages = convert_descriptors_to_messages(stored);
        let tools = self.catalog.tools_for_profile(ToolProfile::Agent);

        Ok((project, api_messages, tools))
    }

    async fn run_chat_loop(
        &self,
        ctx: &mut TickContext,
        project: &aura_tools::domain_tools::ProjectDescriptor,
        cfg: &ChatConfig,
        api_messages: Vec<Message>,
        tools: Vec<aura_reasoner::ToolDefinition>,
    ) -> Result<aura_agent::AgentLoopResult, AutomatonError> {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Waiting for response...".into(),
        })?;

        let project_info = ProjectInfo {
            project_id: None,
            name: &project.name,
            description: project.description.as_deref().unwrap_or(""),
            folder_path: &project.path,
            build_command: project.build_command.as_deref(),
            test_command: project.test_command.as_deref(),
        };

        // Chat shares the dev-loop / task-run advisory drain pattern;
        // see `dev_loop::forward_event` for the post-E.4 drop policy
        // that keeps the high-cadence streaming-pump events from
        // flooding the operator log.
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(1024);
        let _forwarder =
            super::dev_loop::spawn_agent_event_forwarder(ctx.event_tx.clone(), event_rx, None);

        let cancel = ctx.cancellation_token().clone();
        let executor = NoOpExecutor;

        let result = self
            .runner
            .execute_chat(
                self.provider.as_ref(),
                &executor,
                &project_info,
                &cfg.custom_system_prompt,
                api_messages,
                tools,
                Some(event_tx),
                Some(cancel),
            )
            .await
            .map_err(|e| AutomatonError::agent_execution(None, e))?;

        info!(
            input_tokens = result.total_input_tokens,
            output_tokens = result.total_output_tokens,
            iterations = result.iterations,
            "chat loop finished"
        );

        Ok(result)
    }

    async fn save_assistant_reply(
        &self,
        ctx: &mut TickContext,
        cfg: &ChatConfig,
        result: &aura_agent::AgentLoopResult,
    ) -> Result<(), AutomatonError> {
        if result.total_text.is_empty() {
            return Ok(());
        }

        let session = self
            .domain
            .get_active_session(&cfg.instance_id)
            .await
            .ok()
            .flatten();
        let session_id = session.map(|s| s.id).unwrap_or_default();

        if let Err(e) = self
            .domain
            .save_message(SaveMessageParams {
                project_id: cfg.project_id.clone(),
                instance_id: cfg.instance_id.clone(),
                session_id,
                role: "assistant".into(),
                content: result.total_text.clone(),
            })
            .await
        {
            error!(error = %e, "failed to save assistant message");
        }

        ctx.emit(AutomatonEvent::MessageSaved {
            message_id: String::new(),
        })?;
        Ok(())
    }
}

/// Convert `MessageDescriptor`s from `DomainApi` into `aura_reasoner::Message`s.
fn convert_descriptors_to_messages(descriptors: &[MessageDescriptor]) -> Vec<Message> {
    descriptors
        .iter()
        .filter_map(|d| {
            let msg = match d.role.as_str() {
                "user" => Message::user(&d.content),
                "assistant" => Message::assistant(&d.content),
                _ => return None,
            };
            Some(msg)
        })
        .collect()
}
