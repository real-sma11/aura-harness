//! Spec-generation automaton.
//!
//! Replaces `SpecGenerationService::generate_specs_streaming()` from
//! `aura-app`. On-demand: a single tick generates specs for a project's
//! requirements document, saves them via `DomainApi`, and returns `Done`.

use std::sync::Arc;

use tracing::{error, info};

use aura_agent::RecordingModelProvider;
use aura_reasoner::ModelProvider;
use aura_tools::domain_tools::DomainApi;

use crate::context::TickContext;
use crate::error::AutomatonError;
use crate::events::AutomatonEvent;
use crate::runtime::{Automaton, TickOutcome};
use crate::schedule::Schedule;

pub struct SpecGenAutomaton {
    domain: Arc<dyn DomainApi>,
    provider: Arc<dyn ModelProvider>,
}

impl SpecGenAutomaton {
    /// Construct a spec-generation automaton bound to a kernel-mediated
    /// model provider.
    ///
    /// The `RecordingModelProvider` bound is sealed in `aura-agent`
    /// (Invariant §1 / §3): the only public way to satisfy it is to
    /// pass an `Arc<aura_agent::KernelModelGateway>`. External callers
    /// therefore cannot hand a raw HTTP `ModelProvider` directly to the
    /// automaton and bypass kernel-side recording.
    pub fn new<P>(domain: Arc<dyn DomainApi>, provider: Arc<P>) -> Self
    where
        P: RecordingModelProvider + Send + Sync + 'static,
    {
        let provider: Arc<dyn ModelProvider> = provider;
        Self { domain, provider }
    }
}

struct SpecGenConfig {
    project_id: String,
}

impl SpecGenConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, AutomatonError> {
        let project_id = config
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AutomatonError::InvalidConfig("missing project_id".into()))?
            .to_string();
        Ok(Self { project_id })
    }
}

const SPEC_GENERATION_SYSTEM_PROMPT: &str = r#"You are a software specification writer. Given a requirements document, break it down into clear, actionable technical specifications.

Each specification should:
1. Have a clear title
2. Contain detailed implementation instructions in markdown
3. Be self-contained enough for a developer to implement independently
4. Be ordered logically (foundational specs first)

Output your response as a JSON array where each element has:
- "title": string
- "markdown_contents": string (detailed spec in markdown)

Output ONLY the JSON array, no other text."#;

use super::common::{run_auxiliary_model_call, AuxiliaryModelCall};

#[async_trait::async_trait]
impl Automaton for SpecGenAutomaton {
    fn kind(&self) -> &'static str {
        "spec-gen"
    }

    fn default_schedule(&self) -> Schedule {
        Schedule::OnDemand
    }

    async fn tick(&self, ctx: &mut TickContext) -> Result<TickOutcome, AutomatonError> {
        let cfg = SpecGenConfig::from_json(&ctx.config)?;

        let requirements = self.load_requirements(ctx, &cfg).await?;
        let specs = self.generate_specs(ctx, &cfg, &requirements).await?;
        self.save_specs(ctx, &cfg, &specs).await?;

        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: format!("{} specs generated and saved", specs.len()),
        })?;

        Ok(TickOutcome::Done)
    }
}

impl SpecGenAutomaton {
    async fn load_requirements(
        &self,
        ctx: &mut TickContext,
        cfg: &SpecGenConfig,
    ) -> Result<String, AutomatonError> {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Loading project...".into(),
        })?;

        let _project = self
            .domain
            .get_project(&cfg.project_id, None)
            .await
            .map_err(|e| AutomatonError::domain_api(None, e))?;

        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Reading requirements document...".into(),
        })?;

        let requirements_path = ctx
            .config
            .get("requirements_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if requirements_path.is_empty() {
            return Err(AutomatonError::InvalidConfig(
                "no requirements_path configured".into(),
            ));
        }

        let requirements = read_requirements(ctx, requirements_path).await?;

        info!(
            project_id = %cfg.project_id,
            bytes = requirements.len(),
            "requirements loaded"
        );

        Ok(requirements)
    }

    async fn generate_specs(
        &self,
        ctx: &mut TickContext,
        cfg: &SpecGenConfig,
        requirements: &str,
    ) -> Result<Vec<ParsedSpec>, AutomatonError> {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Generating specifications...".into(),
        })?;

        // No silent fallback: spec generation must use the
        // caller-selected model. Falling back to a build-time
        // constant here was one of the paths that produced the
        // opus-4-6 vs opus-4-7 routing regression.
        let model = ctx
            .config
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AutomatonError::InvalidConfig(
                    "missing model — spec generation requires an explicit model identifier in the start request".into(),
                )
            })?
            .to_string();

        let response = run_auxiliary_model_call(
            self.provider.as_ref(),
            AuxiliaryModelCall {
                model: &model,
                system_prompt: SPEC_GENERATION_SYSTEM_PROMPT,
                user_body: requirements.to_string(),
                max_tokens: aura_config::agent().automaton.spec_gen_max_tokens,
                task_scope: None,
            },
        )
        .await?;

        ctx.emit(AutomatonEvent::TokenUsage {
            task_id: None,
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
        })?;

        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: "Parsing AI response...".into(),
        })?;

        let response_text = response.message.text_content();
        let specs = parse_spec_response(&response_text)?;
        info!(
            project_id = %cfg.project_id,
            count = specs.len(),
            "parsed specs from LLM response"
        );

        Ok(specs)
    }

    async fn save_specs(
        &self,
        ctx: &mut TickContext,
        cfg: &SpecGenConfig,
        specs: &[ParsedSpec],
    ) -> Result<(), AutomatonError> {
        ctx.emit(AutomatonEvent::Progress {
            task_id: None,
            message: format!("Saving {} specs...", specs.len()),
        })?;

        let existing = self
            .domain
            .list_specs(&cfg.project_id, None)
            .await
            .unwrap_or_default();
        for s in &existing {
            if let Err(e) = self.domain.delete_spec(&s.id, None).await {
                error!(spec_id = %s.id, error = %e, "failed to delete old spec");
            }
        }

        for (idx, spec) in specs.iter().enumerate() {
            let saved = self
                .domain
                .create_spec(
                    &cfg.project_id,
                    &spec.title,
                    &spec.content,
                    idx as u32,
                    None,
                )
                .await
                .map_err(|e| AutomatonError::domain_api(None, e.context("save spec")))?;

            ctx.emit(AutomatonEvent::SpecSaved {
                spec_id: saved.id,
                title: saved.title,
            })?;
        }

        Ok(())
    }
}

async fn read_requirements(
    ctx: &TickContext,
    requirements_path: &str,
) -> Result<String, AutomatonError> {
    let workspace_root = ctx
        .workspace_root
        .as_deref()
        .ok_or_else(|| AutomatonError::InvalidConfig("no workspace_root set".into()))?;
    let resolved = workspace_root.join(requirements_path);
    let canonical = resolved.canonicalize().map_err(|e| {
        AutomatonError::InvalidConfig(format!(
            "failed to resolve requirements path {requirements_path}: {e}"
        ))
    })?;
    let canonical_base = workspace_root.canonicalize().map_err(|e| {
        AutomatonError::InvalidConfig(format!("failed to canonicalize workspace root: {e}"))
    })?;
    if !canonical.starts_with(&canonical_base) {
        return Err(AutomatonError::InvalidConfig(format!(
            "requirements_path escapes workspace root: {requirements_path}"
        )));
    }
    tokio::fs::read_to_string(&canonical).await.map_err(|e| {
        AutomatonError::InvalidConfig(format!(
            "failed to read requirements file {requirements_path}: {e}"
        ))
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
struct RawSpec {
    title: String,
    markdown_contents: String,
}

struct ParsedSpec {
    title: String,
    content: String,
}

fn parse_spec_response(text: &str) -> Result<Vec<ParsedSpec>, AutomatonError> {
    let trimmed = text.trim();

    let json_str = if let Some(s) = extract_fenced_json(trimmed) {
        s
    } else {
        trimmed.to_string()
    };

    let raw: Vec<RawSpec> = serde_json::from_str(&json_str).map_err(|e| {
        AutomatonError::agent_execution(
            None,
            aura_agent::AgentError::Internal(format!("failed to parse spec JSON: {e}")),
        )
    })?;

    if raw.is_empty() {
        return Err(AutomatonError::agent_execution(
            None,
            aura_agent::AgentError::Internal("LLM returned empty spec array".into()),
        ));
    }

    Ok(raw
        .into_iter()
        .map(|r| ParsedSpec {
            title: r.title,
            content: r.markdown_contents,
        })
        .collect())
}

fn extract_fenced_json(text: &str) -> Option<String> {
    let start = text.find("```")?;
    let after_fence = &text[start + 3..];
    let content_start = after_fence.find('\n')? + 1;
    let rest = &after_fence[content_start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}
