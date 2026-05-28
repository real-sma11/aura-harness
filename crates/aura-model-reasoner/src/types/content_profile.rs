use super::content::{ContentBlock, Role, ToolResultContent};
use super::request::ModelRequest;
use super::tool::ToolChoice;
use serde::{Deserialize, Serialize};

const DEV_LOOP_BOOTSTRAP_LAST_USER_MAX_BYTES: usize = 16 * 1024;
const DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES: usize = 24 * 1024;
const PROJECT_TOOL_LAST_USER_MAX_BYTES: usize = 32 * 1024;
const PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES: usize = 48 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRequestKind {
    Chat,
    ProjectToolSpecGen,
    ProjectToolTaskExtract,
    DevLoopBootstrap,
    DevLoopContinuation,
    Auxiliary,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ModelRequestMetadata {
    pub kind: Option<ModelRequestKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelContractVerdict {
    Accept,
    Warn,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelContractViolationReason {
    MissingStableSessionId,
    UnboundedBootstrapContext,
    EmergencyCapRequired,
    OversizedToolResult,
    UnknownRequestKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestContractViolation {
    pub reason: ModelContractViolationReason,
    pub profile: ModelContentProfile,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelContentProfile {
    pub kind: ModelRequestKind,
    pub verdict: ModelContractVerdict,
    pub reasons: Vec<ModelContractViolationReason>,
    pub has_aura_project_id: bool,
    pub has_aura_agent_id: bool,
    pub has_aura_session_id: bool,
    pub has_aura_org_id: bool,
    pub system_bytes: usize,
    pub messages_text_bytes: usize,
    pub last_user_text_bytes: usize,
    pub max_tool_result_bytes: usize,
    pub tool_count: usize,
    pub tool_names: Vec<String>,
    pub content_signature: String,
}

impl ModelContentProfile {
    #[must_use]
    pub fn from_request(request: &ModelRequest) -> Self {
        let has_aura_project_id = has_stable_value(request.aura_project_id.as_deref());
        let has_aura_agent_id = has_stable_value(request.aura_agent_id.as_deref());
        let has_aura_session_id = has_stable_value(request.aura_session_id.as_deref());
        let has_aura_org_id = has_stable_value(request.aura_org_id.as_deref());
        let mut messages_text_bytes = 0usize;
        let mut last_user_text_bytes = 0usize;
        let mut max_tool_result_bytes = 0usize;

        for message in &request.messages {
            let mut message_text_bytes = 0usize;
            for block in &message.content {
                match block {
                    ContentBlock::Text { text } => {
                        messages_text_bytes += text.len();
                        message_text_bytes += text.len();
                    }
                    ContentBlock::ToolResult { content, .. } => {
                        let len = tool_result_len(content);
                        messages_text_bytes += len;
                        message_text_bytes += len;
                        max_tool_result_bytes = max_tool_result_bytes.max(len);
                    }
                    ContentBlock::Thinking { thinking, .. } => {
                        messages_text_bytes += thinking.len();
                        message_text_bytes += thinking.len();
                    }
                    ContentBlock::ToolUse { input, .. } => {
                        let len = serde_json::to_string(input).map_or(0, |s| s.len());
                        messages_text_bytes += len;
                        message_text_bytes += len;
                    }
                    ContentBlock::Image { source } => {
                        messages_text_bytes += source.data.len();
                        message_text_bytes += source.data.len();
                    }
                }
            }
            if message.role == Role::User {
                last_user_text_bytes = message_text_bytes;
            }
        }

        let tool_names = request.tools.iter().map(|tool| tool.name.clone()).collect();
        let mut profile = Self {
            kind: infer_request_kind(request),
            verdict: ModelContractVerdict::Accept,
            reasons: Vec::new(),
            has_aura_project_id,
            has_aura_agent_id,
            has_aura_session_id,
            has_aura_org_id,
            system_bytes: request.system.len(),
            messages_text_bytes,
            last_user_text_bytes,
            max_tool_result_bytes,
            tool_count: request.tools.len(),
            tool_names,
            content_signature: content_signature(request),
        };
        profile.apply_contract();
        profile
    }

    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "kind={:?}; verdict={:?}; reasons={:?}; content_signature={}; \
             system_bytes={}; messages_text_bytes={}; last_user_text_bytes={}; \
             tools={}; aura_session_id={}",
            self.kind,
            self.verdict,
            self.reasons,
            self.content_signature,
            self.system_bytes,
            self.messages_text_bytes,
            self.last_user_text_bytes,
            self.tool_count,
            if self.has_aura_session_id {
                "present"
            } else {
                "missing"
            }
        )
    }

    /// Drop into either `Ok(self)` (accept/warn) or
    /// `Err(Box<ModelRequestContractViolation>)` (block). Boxed to
    /// keep the carrier under 128 bytes — `ModelContentProfile` is
    /// large enough on its own that the unboxed `Result` trips
    /// `clippy::result_large_err` everywhere.
    pub fn validate(self) -> Result<Self, Box<ModelRequestContractViolation>> {
        if self.verdict != ModelContractVerdict::Block {
            return Ok(self);
        }
        let reason = self
            .reasons
            .first()
            .copied()
            .unwrap_or(ModelContractViolationReason::UnknownRequestKind);
        Err(Box::new(ModelRequestContractViolation {
            reason,
            message: format!("model request contract blocked: {}", self.summary()),
            profile: self,
        }))
    }

    fn apply_contract(&mut self) {
        if self.requires_stable_session() && !self.has_aura_session_id {
            self.reasons
                .push(ModelContractViolationReason::MissingStableSessionId);
        }

        match self.kind {
            ModelRequestKind::DevLoopBootstrap => {
                if self.last_user_text_bytes > DEV_LOOP_BOOTSTRAP_LAST_USER_MAX_BYTES
                    || self.messages_text_bytes > DEV_LOOP_BOOTSTRAP_TOTAL_TEXT_MAX_BYTES
                {
                    self.reasons
                        .push(ModelContractViolationReason::UnboundedBootstrapContext);
                }
            }
            ModelRequestKind::ProjectToolSpecGen | ModelRequestKind::ProjectToolTaskExtract => {
                if self.last_user_text_bytes > PROJECT_TOOL_LAST_USER_MAX_BYTES
                    || self.messages_text_bytes > PROJECT_TOOL_TOTAL_TEXT_MAX_BYTES
                {
                    self.reasons
                        .push(ModelContractViolationReason::EmergencyCapRequired);
                }
            }
            ModelRequestKind::Chat
            | ModelRequestKind::DevLoopContinuation
            | ModelRequestKind::Auxiliary => {}
        }

        self.verdict = if self.reasons.is_empty() {
            ModelContractVerdict::Accept
        } else {
            ModelContractVerdict::Block
        };
    }

    fn requires_stable_session(&self) -> bool {
        matches!(
            self.kind,
            ModelRequestKind::ProjectToolSpecGen
                | ModelRequestKind::ProjectToolTaskExtract
                | ModelRequestKind::DevLoopBootstrap
                | ModelRequestKind::DevLoopContinuation
        )
    }
}

impl std::fmt::Display for ModelRequestContractViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

fn infer_request_kind(request: &ModelRequest) -> ModelRequestKind {
    if let Some(kind) = request.metadata.kind {
        return kind;
    }

    // Adapter gap: callers that do not yet set `ModelRequestMetadata.kind` are
    // inferred conservatively from stable router identity and tool surface only.
    // This avoids disrupting direct memory/kernel calls while still catching the
    // project-tool/task-extraction and dev-loop shapes that Phase 2 identified.
    if has_project_task_tools(request) {
        return ModelRequestKind::ProjectToolTaskExtract;
    }

    let has_identity = has_stable_value(request.aura_project_id.as_deref())
        || has_stable_value(request.aura_agent_id.as_deref())
        || has_stable_value(request.aura_org_id.as_deref())
        || has_stable_value(request.aura_session_id.as_deref());

    if has_identity && !request.tools.is_empty() {
        if is_single_user_bootstrap(request) {
            ModelRequestKind::DevLoopBootstrap
        } else {
            ModelRequestKind::DevLoopContinuation
        }
    } else if matches!(request.tool_choice, ToolChoice::None) && request.tools.is_empty() {
        ModelRequestKind::Chat
    } else {
        ModelRequestKind::Auxiliary
    }
}

fn is_single_user_bootstrap(request: &ModelRequest) -> bool {
    request.messages.len() == 1
        && request
            .messages
            .first()
            .is_some_and(|message| message.role == Role::User)
}

fn has_project_task_tools(request: &ModelRequest) -> bool {
    request.tools.iter().any(|tool| {
        matches!(
            tool.name.as_str(),
            "create_task" | "update_task" | "list_tasks" | "get_task" | "delete_task"
        )
    })
}

fn tool_result_len(content: &ToolResultContent) -> usize {
    match content {
        ToolResultContent::Text(text) => text.len(),
        ToolResultContent::Json(value) => serde_json::to_string(value).map_or(0, |s| s.len()),
    }
}

fn has_stable_value(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| !value.is_empty())
}

fn content_signature(request: &ModelRequest) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    fn write(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    write(&mut hash, request.model.as_str().as_bytes());
    write(&mut hash, request.system.as_bytes());
    for message in &request.messages {
        write(&mut hash, format!("{:?}", message.role).as_bytes());
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => write(&mut hash, text.as_bytes()),
                ContentBlock::Thinking { thinking, .. } => write(&mut hash, thinking.as_bytes()),
                ContentBlock::Image { source } => write(&mut hash, source.data.as_bytes()),
                ContentBlock::ToolUse { id, name, input } => {
                    write(&mut hash, id.as_bytes());
                    write(&mut hash, name.as_bytes());
                    if let Ok(json) = serde_json::to_string(input) {
                        write(&mut hash, json.as_bytes());
                    }
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    write(&mut hash, tool_use_id.as_bytes());
                    match content {
                        ToolResultContent::Text(text) => write(&mut hash, text.as_bytes()),
                        ToolResultContent::Json(value) => {
                            if let Ok(json) = serde_json::to_string(value) {
                                write(&mut hash, json.as_bytes());
                            }
                        }
                    }
                }
            }
        }
    }
    format!("{hash:016x}")
}
