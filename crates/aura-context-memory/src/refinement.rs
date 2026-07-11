//! Stage 2: LLM-powered fact extraction and refinement.
//!
//! Combines conversation-aware extraction with heuristic candidate refinement
//! in a single cheap LLM call (Haiku by default).

use crate::error::MemoryError;
use crate::extraction::ConversationTurn;
use crate::types::{CandidateType, MemoryCandidate, RefinedCandidate};
use aura_model_reasoner::{Message, ModelProvider, ModelRequest};
use std::fmt::Write;
use std::sync::Arc;

const MAX_TURN_TEXT_LEN: usize = 3000;

/// LLM-assisted memory refiner.
///
/// Accepts any [`ModelProvider`] but production wiring MUST pass a
/// recording-capable provider (e.g. `aura_agent::KernelModelGateway`).
/// The runtime composition root in `aura_runtime::node` is the single
/// place that knows about the kernel gateway, and it is responsible for
/// upholding Invariant §3 ("Every LLM Call Is Recorded") by passing a
/// kernel-mediated provider here. The context layer cannot enforce this
/// structurally without re-introducing an upward edge into `aura-agent`.
pub struct LlmRefiner {
    provider: Arc<dyn ModelProvider + Send + Sync>,
    config: RefinerConfig,
}

pub struct RefinerConfig {
    pub model: String,
    pub auth_token: Option<String>,
}

/// Per-turn routing identity for memory extraction requests.
///
/// Memory refinement is an auxiliary model call, but it still belongs to the
/// originating Aura request for authorization, billing, and observability.
/// Keeping this context per call avoids leaking one session's identity into
/// another when the shared memory manager handles concurrent agents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefinementRequestContext {
    pub auth_token: Option<String>,
    pub aura_project_id: Option<String>,
    pub aura_agent_id: Option<String>,
    pub aura_session_id: Option<String>,
    pub aura_org_id: Option<String>,
}

impl Default for RefinerConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            auth_token: None,
        }
    }
}

impl LlmRefiner {
    #[must_use]
    pub fn model_name(&self) -> &str {
        &self.config.model
    }

    /// Construct a refiner that routes all refinement / extraction
    /// completions through `provider`.
    ///
    /// `provider` must be a recording-capable [`ModelProvider`] —
    /// typically `aura_agent::KernelModelGateway` — so Invariant §3
    /// ("Every LLM Call Is Recorded") is preserved end-to-end. The
    /// context layer accepts the abstract trait object to keep the
    /// crate free of any upward edge into `aura-agent`; the runtime
    /// composition root is responsible for picking the right impl.
    pub fn new(provider: Arc<dyn ModelProvider + Send + Sync>, config: RefinerConfig) -> Self {
        Self { provider, config }
    }

    /// Extract facts from a conversation turn AND refine heuristic candidates
    /// in a single LLM call.
    ///
    /// When no conversation turn is available (e.g. automated runs with no
    /// user message), falls back to refining heuristic candidates only.
    ///
    /// # Errors
    /// Returns error on provider failure or unparseable response.
    pub async fn extract_and_refine(
        &self,
        candidates: Vec<MemoryCandidate>,
        turn: Option<&ConversationTurn>,
        auth_token_override: Option<String>,
    ) -> Result<Vec<RefinedCandidate>, MemoryError> {
        self.extract_and_refine_with_skills(candidates, turn, auth_token_override, &[])
            .await
    }

    pub async fn extract_and_refine_with_skills(
        &self,
        candidates: Vec<MemoryCandidate>,
        turn: Option<&ConversationTurn>,
        auth_token_override: Option<String>,
        active_skills: &[String],
    ) -> Result<Vec<RefinedCandidate>, MemoryError> {
        self.extract_and_refine_with_skills_and_context(
            candidates,
            turn,
            RefinementRequestContext {
                auth_token: auth_token_override,
                ..Default::default()
            },
            active_skills,
        )
        .await
    }

    pub async fn extract_and_refine_with_skills_and_context(
        &self,
        candidates: Vec<MemoryCandidate>,
        turn: Option<&ConversationTurn>,
        request_context: RefinementRequestContext,
        active_skills: &[String],
    ) -> Result<Vec<RefinedCandidate>, MemoryError> {
        if candidates.is_empty() && turn.is_none() {
            return Ok(Vec::new());
        }

        let prompt = Self::build_extraction_prompt(&candidates, turn, active_skills);
        let request = self.build_request(prompt, request_context)?;

        let response = self
            .provider
            .complete(request)
            .await
            .map_err(|e| MemoryError::Provider(e.to_string()))?;

        let response_text = response.message.text_content();
        Ok(Self::parse_response(&response_text, &candidates))
    }

    fn build_request(
        &self,
        prompt: String,
        request_context: RefinementRequestContext,
    ) -> Result<ModelRequest, MemoryError> {
        let effective_token = request_context
            .auth_token
            .or_else(|| self.config.auth_token.clone());
        ModelRequest::builder(&self.config.model, EXTRACTOR_SYSTEM_PROMPT)
            .messages(vec![Message::user(prompt)])
            .max_tokens(1024)
            .auth_token(effective_token)
            .aura_project_id(request_context.aura_project_id)
            .aura_agent_id(request_context.aura_agent_id)
            .aura_session_id(request_context.aura_session_id)
            .aura_org_id(request_context.aura_org_id)
            .try_build()
            .map_err(|e| MemoryError::Provider(e.to_string()))
    }

    /// Backward-compatible entry point that only refines existing candidates
    /// without conversation context.
    pub async fn refine(
        &self,
        candidates: Vec<MemoryCandidate>,
    ) -> Result<Vec<RefinedCandidate>, MemoryError> {
        self.extract_and_refine(candidates, None, None).await
    }

    fn build_extraction_prompt(
        candidates: &[MemoryCandidate],
        turn: Option<&ConversationTurn>,
        active_skills: &[String],
    ) -> String {
        let mut prompt = String::new();

        if let Some(turn) = turn {
            prompt.push_str("## Conversation turn\n\n");
            prompt.push_str("User: ");
            let user_msg = truncate(&turn.user_message, MAX_TURN_TEXT_LEN);
            prompt.push_str(&user_msg);
            prompt.push_str("\n\nAssistant: ");
            let assistant_msg = truncate(&turn.assistant_text, MAX_TURN_TEXT_LEN);
            prompt.push_str(&assistant_msg);
            prompt.push_str("\n\n");
        }

        if !active_skills.is_empty() {
            prompt.push_str("## Active skills\n\n");
            for skill in active_skills {
                let _ = writeln!(prompt, "- {skill}");
            }
            prompt.push('\n');
        }

        if !candidates.is_empty() {
            prompt.push_str("## Pre-extracted candidates\n\n");
            for (i, c) in candidates.iter().enumerate() {
                let type_str = match c.candidate_type {
                    CandidateType::Fact => "fact",
                    CandidateType::Event => "event",
                    CandidateType::Procedure => "procedure",
                };
                let key_str = c.key.as_deref().unwrap_or("(none)");
                let summary_str = c.summary.as_deref().unwrap_or("");
                let _ = writeln!(
                    prompt,
                    "{}. {}: key={}, value={}, source={} {}",
                    i + 1,
                    type_str,
                    key_str,
                    c.value,
                    c.source_hint,
                    summary_str
                );
            }
            prompt.push('\n');
        }

        prompt.push_str("## Instructions\n\n");

        if !candidates.is_empty() {
            prompt.push_str(
                "For each pre-extracted candidate, respond with one line:\n\
                 N. KEEP|DROP key=\"refined_key\" confidence=0.X importance=0.X\n\n",
            );
        }

        prompt.push_str(
            "If the conversation contains facts worth remembering long-term, \
             output additional lines:\n\
             FACT key=\"snake_case_key\" value=\"the fact\" confidence=0.X importance=0.X\n\n\
             If the user states a standing instruction, workflow preference, or \
             \"always do X when Y\" rule, output:\n\
             PROCEDURE name=\"snake_case_name\" trigger=\"when this happens\" \
             steps=\"step1;step2\" skill=\"skill_name_or_none\" \
             confidence=0.X importance=0.X\n\n\
             If there are no new facts or procedures to extract, output nothing extra.",
        );

        prompt
    }

    fn parse_response(response: &str, candidates: &[MemoryCandidate]) -> Vec<RefinedCandidate> {
        let mut refined = Vec::new();
        let mut seen_indices = Vec::new();

        for line in response.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if line.starts_with("PROCEDURE ") {
                if let Some(candidate) = parse_procedure_line(line) {
                    refined.push(candidate);
                    continue;
                }
            }

            if line.starts_with("FACT ") {
                if let Some(candidate) = parse_fact_line(line) {
                    refined.push(candidate);
                    continue;
                }
            }

            // Try parsing as a numbered candidate refinement (N. KEEP|DROP ...)
            let parts: Vec<&str> = line.splitn(2, ". ").collect();
            if parts.len() != 2 {
                continue;
            }
            let idx: usize = match parts[0].trim().parse::<usize>() {
                Ok(n) if n >= 1 && n <= candidates.len() => n - 1,
                _ => continue,
            };

            let rest = parts[1];
            let keep = rest.starts_with("KEEP");

            let confidence = extract_float(rest, "confidence=")
                .unwrap_or(candidates[idx].preliminary_confidence);
            let importance = extract_float(rest, "importance=")
                .unwrap_or(candidates[idx].preliminary_importance);
            let key = extract_quoted(rest, "key=")
                .unwrap_or_else(|| candidates[idx].key.clone().unwrap_or_default());

            seen_indices.push(idx);
            refined.push(RefinedCandidate {
                candidate_type: candidates[idx].candidate_type.clone(),
                key,
                value: candidates[idx].value.clone(),
                summary: candidates[idx].summary.clone(),
                confidence,
                importance,
                keep,
                trigger: None,
                steps: None,
                skill_name: None,
            });
        }

        // Candidates not addressed by the LLM are kept by default
        for (i, c) in candidates.iter().enumerate() {
            if !seen_indices.contains(&i) {
                refined.push(RefinedCandidate {
                    candidate_type: c.candidate_type.clone(),
                    key: c.key.clone().unwrap_or_default(),
                    value: c.value.clone(),
                    summary: c.summary.clone(),
                    confidence: c.preliminary_confidence,
                    importance: c.preliminary_importance,
                    keep: true,
                    trigger: None,
                    steps: None,
                    skill_name: None,
                });
            }
        }

        refined
    }
}

fn parse_fact_line(line: &str) -> Option<RefinedCandidate> {
    let key = extract_quoted(line, "key=")?;
    let value = extract_quoted(line, "value=")?;
    let confidence = extract_float(line, "confidence=").unwrap_or(0.8);
    let importance = extract_float(line, "importance=").unwrap_or(0.5);

    Some(RefinedCandidate {
        candidate_type: CandidateType::Fact,
        key,
        value: serde_json::Value::String(value),
        summary: None,
        confidence,
        importance,
        keep: true,
        trigger: None,
        steps: None,
        skill_name: None,
    })
}

fn parse_procedure_line(line: &str) -> Option<RefinedCandidate> {
    let name = extract_quoted(line, "name=")?;
    let trigger = extract_quoted(line, "trigger=")?;
    let steps_raw = extract_quoted(line, "steps=").unwrap_or_default();
    let skill = extract_quoted(line, "skill=").unwrap_or_else(|| "none".to_string());
    let confidence = extract_float(line, "confidence=").unwrap_or(0.8);
    let importance = extract_float(line, "importance=").unwrap_or(0.7);

    let steps: Vec<String> = steps_raw
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let skill_name = if skill.eq_ignore_ascii_case("none") || skill.is_empty() {
        None
    } else {
        Some(skill)
    };

    Some(RefinedCandidate {
        candidate_type: CandidateType::Procedure,
        key: name,
        value: serde_json::json!({ "trigger": trigger, "steps": steps }),
        summary: Some(trigger.clone()),
        confidence,
        importance,
        keep: true,
        trigger: Some(trigger),
        steps: Some(steps),
        skill_name,
    })
}

fn extract_float(text: &str, prefix: &str) -> Option<f32> {
    let start = text.find(prefix)? + prefix.len();
    let end = text[start..]
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .map_or(text.len(), |e| start + e);
    text[start..end].parse().ok()
}

fn extract_quoted(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)? + prefix.len();
    let rest = &text[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }
}

const EXTRACTOR_SYSTEM_PROMPT: &str = "\
You are a memory system for an AI agent. After each conversation turn you \
extract facts and procedures worth remembering long-term and refine any \
pre-extracted candidates.

What to extract as FACT:
- Explicit user preferences (favorite tools, languages, pets, etc.)
- Personal facts the user shares (name, role, company, location)
- Technical decisions and project details
- Constraints or requirements stated by the user

What to extract as PROCEDURE:
- Standing instructions (\"always do X\", \"going forward, do Y\")
- Workflow preferences (where to store files, how to format things)
- Skill-specific conventions (\"use obsidian for notes\", etc.)
- Repeated patterns the user wants followed every time

What NOT to extract:
- Transient requests ('fix this bug', 'run the tests')
- Greetings or conversational filler
- Information already covered by a pre-extracted candidate

Output format — one line per item:
- For pre-extracted candidates: N. KEEP|DROP key=\"refined_key\" confidence=0.X importance=0.X
- For newly extracted facts: FACT key=\"snake_case_key\" value=\"concise fact\" confidence=0.X importance=0.X
- For procedures: PROCEDURE name=\"snake_case_name\" trigger=\"when condition\" steps=\"step1;step2\" skill=\"skill_name_or_none\" confidence=0.X importance=0.X

For PROCEDURE: set skill= to the relevant active skill name if one applies, \
otherwise use \"none\". Steps are semicolon-separated.

Be selective. Only extract items that would be useful across future sessions.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CandidateType, MemoryCandidate};
    use aura_model_reasoner::MockProvider;

    fn make_candidate(key: &str) -> MemoryCandidate {
        MemoryCandidate {
            candidate_type: CandidateType::Fact,
            key: Some(key.to_string()),
            value: serde_json::Value::String("val".to_string()),
            summary: None,
            source_hint: "test".to_string(),
            preliminary_confidence: 0.7,
            preliminary_importance: 0.5,
        }
    }

    #[test]
    fn extract_float_present() {
        assert!(
            (extract_float("confidence=0.85 rest", "confidence=").unwrap() - 0.85).abs() < 1e-3
        );
    }

    #[test]
    fn extract_float_missing() {
        assert!(extract_float("no match", "confidence=").is_none());
    }

    #[test]
    fn extract_float_malformed() {
        assert!(extract_float("confidence=abc", "confidence=").is_none());
    }

    #[test]
    fn extract_quoted_double_quoted() {
        let result = extract_quoted("key=\"hello world\" rest", "key=");
        assert_eq!(result.unwrap(), "hello world");
    }

    #[test]
    fn extract_quoted_unquoted() {
        let result = extract_quoted("key=bare_value rest", "key=");
        assert_eq!(result.unwrap(), "bare_value");
    }

    #[test]
    fn extract_quoted_missing() {
        assert!(extract_quoted("no match", "key=").is_none());
    }

    #[test]
    fn parse_response_valid_keep_drop() {
        let candidates = vec![make_candidate("a"), make_candidate("b")];
        let response = "1. KEEP key=\"alpha\" confidence=0.9 importance=0.8\n2. DROP key=\"beta\" confidence=0.3 importance=0.1";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 2);
        assert!(refined[0].keep);
        assert!(!refined[1].keep);
        assert_eq!(refined[0].key, "alpha");
    }

    #[test]
    fn parse_response_out_of_range_index_ignored() {
        let candidates = vec![make_candidate("a")];
        let response = "1. KEEP key=\"a\" confidence=0.9 importance=0.8\n5. KEEP key=\"bad\" confidence=0.9 importance=0.8";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 1);
    }

    #[test]
    fn parse_response_malformed_lines_skipped() {
        let candidates = vec![make_candidate("a")];
        let response = "garbage\n1. KEEP key=\"a\" confidence=0.9 importance=0.8\nmore garbage";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 1);
        assert!(refined[0].keep);
    }

    #[test]
    fn parse_response_empty() {
        let candidates = vec![make_candidate("a"), make_candidate("b")];
        let response = "";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 2);
        assert!(refined.iter().all(|r| r.keep));
    }

    #[test]
    fn parse_fact_line_valid() {
        let line =
            "FACT key=\"favorite_dog\" value=\"Belgian Malanois\" confidence=0.95 importance=0.6";
        let result = parse_fact_line(line).unwrap();
        assert_eq!(result.key, "favorite_dog");
        assert_eq!(
            result.value,
            serde_json::Value::String("Belgian Malanois".into())
        );
        assert!((result.confidence - 0.95).abs() < 1e-3);
        assert!(result.keep);
    }

    #[test]
    fn parse_response_with_new_facts() {
        let candidates = vec![make_candidate("a")];
        let response = "1. KEEP key=\"a\" confidence=0.9 importance=0.8\nFACT key=\"pet\" value=\"dog\" confidence=0.85 importance=0.5";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 2);
        assert_eq!(refined[1].key, "pet");
    }

    #[test]
    fn build_extraction_prompt_with_turn() {
        let turn = ConversationTurn {
            user_message: "My favorite dog is a Belgian Malanois".to_string(),
            assistant_text: "Great choice!".to_string(),
        };
        let prompt = LlmRefiner::build_extraction_prompt(&[], Some(&turn), &[]);
        assert!(prompt.contains("Belgian Malanois"));
        assert!(prompt.contains("Great choice!"));
        assert!(prompt.contains("FACT"));
    }

    #[test]
    fn build_extraction_prompt_without_turn() {
        let candidates = vec![make_candidate("test_key")];
        let prompt = LlmRefiner::build_extraction_prompt(&candidates, None, &[]);
        assert!(prompt.contains("test_key"));
        assert!(prompt.contains("KEEP|DROP"));
    }

    #[test]
    fn refinement_request_preserves_aura_routing_context() {
        let refiner = LlmRefiner::new(Arc::new(MockProvider::new()), RefinerConfig::default());
        let request = refiner
            .build_request(
                "extract memory".to_string(),
                RefinementRequestContext {
                    auth_token: Some("jwt".to_string()),
                    aura_project_id: Some("project-123".to_string()),
                    aura_agent_id: Some("agent-123".to_string()),
                    aura_session_id: Some("session-123".to_string()),
                    aura_org_id: Some("org-123".to_string()),
                },
            )
            .expect("request should build");

        assert_eq!(request.auth_token.as_deref(), Some("jwt"));
        assert_eq!(request.aura_project_id.as_deref(), Some("project-123"));
        assert_eq!(request.aura_agent_id.as_deref(), Some("agent-123"));
        assert_eq!(request.aura_session_id.as_deref(), Some("session-123"));
        assert_eq!(request.aura_org_id.as_deref(), Some("org-123"));
    }

    #[test]
    fn build_extraction_prompt_with_skills() {
        let turn = ConversationTurn {
            user_message: "Store notes in obsidian".to_string(),
            assistant_text: "Got it!".to_string(),
        };
        let skills = vec!["obsidian".to_string(), "git".to_string()];
        let prompt = LlmRefiner::build_extraction_prompt(&[], Some(&turn), &skills);
        assert!(prompt.contains("## Active skills"));
        assert!(prompt.contains("- obsidian"));
        assert!(prompt.contains("- git"));
    }

    #[test]
    fn truncate_does_not_split_utf8() {
        let truncated = truncate("abc\u{1F980}def", 5);
        assert_eq!(truncated, "abc...");
    }

    #[test]
    fn parse_procedure_line_valid() {
        let line = "PROCEDURE name=\"save_research\" trigger=\"when creating research reports\" steps=\"Store in Research/ folder;Use YAML frontmatter\" skill=\"obsidian\" confidence=0.9 importance=0.8";
        let result = parse_procedure_line(line).unwrap();
        assert_eq!(result.key, "save_research");
        assert!(matches!(result.candidate_type, CandidateType::Procedure));
        assert_eq!(
            result.trigger.as_deref(),
            Some("when creating research reports")
        );
        assert_eq!(result.steps.as_ref().unwrap().len(), 2);
        assert_eq!(result.skill_name.as_deref(), Some("obsidian"));
        assert!((result.confidence - 0.9).abs() < 1e-3);
        assert!(result.keep);
    }

    #[test]
    fn parse_procedure_line_no_skill() {
        let line = "PROCEDURE name=\"format_code\" trigger=\"when writing code\" steps=\"Use 4-space indent\" skill=\"none\" confidence=0.85 importance=0.6";
        let result = parse_procedure_line(line).unwrap();
        assert_eq!(result.key, "format_code");
        assert!(result.skill_name.is_none());
    }

    #[test]
    fn parse_procedure_line_missing_name() {
        let line = "PROCEDURE trigger=\"when\" steps=\"do stuff\" skill=\"none\" confidence=0.8 importance=0.5";
        assert!(parse_procedure_line(line).is_none());
    }

    #[test]
    fn parse_response_with_procedure() {
        let candidates = vec![make_candidate("a")];
        let response = "1. KEEP key=\"a\" confidence=0.9 importance=0.8\nPROCEDURE name=\"save_notes\" trigger=\"when saving notes\" steps=\"Use obsidian\" skill=\"obsidian\" confidence=0.85 importance=0.7";
        let refined = LlmRefiner::parse_response(response, &candidates);
        assert_eq!(refined.len(), 2);
        assert_eq!(refined[0].key, "a");
        assert_eq!(refined[1].key, "save_notes");
        assert!(matches!(
            refined[1].candidate_type,
            CandidateType::Procedure
        ));
        assert_eq!(refined[1].skill_name.as_deref(), Some("obsidian"));
    }

    #[test]
    fn parse_response_mixed_facts_and_procedures() {
        let response = "FACT key=\"dog_name\" value=\"Kaya\" confidence=0.95 importance=0.6\nPROCEDURE name=\"research_reports\" trigger=\"when creating research reports\" steps=\"Store in Research/ folder\" skill=\"obsidian\" confidence=0.9 importance=0.8";
        let refined = LlmRefiner::parse_response(response, &[]);
        assert_eq!(refined.len(), 2);
        assert!(matches!(refined[0].candidate_type, CandidateType::Fact));
        assert!(matches!(
            refined[1].candidate_type,
            CandidateType::Procedure
        ));
    }
}
