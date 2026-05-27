//! Shared no-op tool executor for automatons that don't execute tools.

pub(crate) struct NoOpExecutor;

#[async_trait::async_trait]
impl aura_agent::types::AgentToolExecutor for NoOpExecutor {
    async fn execute(
        &self,
        tool_calls: &[aura_agent::types::ToolCallInfo],
    ) -> Vec<aura_agent::types::ToolCallResult> {
        tool_calls
            .iter()
            .map(|tc| {
                aura_agent::types::ToolCallResult::error(&tc.id, "no tool executor configured")
            })
            .collect()
    }
}
