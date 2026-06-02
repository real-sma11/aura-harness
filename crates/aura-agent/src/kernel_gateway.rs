//! Kernel gateway adapters that bridge the new Kernel API to existing `AgentLoop` traits.
//!
//! - [`KernelToolGateway`] implements [`AgentToolExecutor`] by routing tool calls
//!   through [`Kernel::process_tools`].
//! - [`KernelModelGateway`] implements [`ModelProvider`] by routing completions
//!   through [`Kernel::reason`] and [`Kernel::reason_streaming`].

use crate::helpers;
use crate::recording_stream::RecordingStream;
use crate::types::{AgentToolExecutor, ToolCallInfo, ToolCallResult};
use async_trait::async_trait;
use aura_agent_kernel::{Kernel, KernelError};
use aura_model_reasoner::{
    ModelProvider, ModelRequest, ModelResponse, ReasonerError, StreamEventStream,
};
use std::sync::Arc;
use tracing::warn;

/// Translate a [`KernelError`] returned by `Kernel::reason*` back into
/// the [`ReasonerError`] expected by the [`ModelProvider`] trait.
///
/// `KernelError::Reasoner` now carries the original `ReasonerError`
/// (rather than a stringified copy), so retry classification in the
/// agent loop can keep matching on `RateLimited`, `InsufficientCredits`,
/// `Api { status }`, etc. without parsing the formatted message. Other
/// kernel-side failure modes (`Timeout`, `Store`, `Serialization`,
/// `Internal`) are mapped to the closest `ReasonerError` variant — they
/// are not provider-side failures and should not be re-classified as
/// rate limits.
fn kernel_error_to_reasoner_error(e: KernelError) -> ReasonerError {
    match e {
        KernelError::Reasoner(inner) => inner,
        KernelError::Timeout(_) => ReasonerError::Timeout,
        other @ (KernelError::Store(_)
        | KernelError::Serialization(_)
        | KernelError::Replay(_)
        | KernelError::Internal(_)) => {
            ReasonerError::Internal(format!("kernel reason error: {other}"))
        }
    }
}

// ============================================================================
// KernelToolGateway
// ============================================================================

/// Routes [`AgentToolExecutor::execute`] through the kernel's batch tool processor.
///
/// Converts `ToolCallInfo` slices into `ToolProposal` vectors, delegates to
/// `Kernel::process_tools`, and maps the results back to `ToolCallResult`.
pub struct KernelToolGateway {
    kernel: Arc<Kernel>,
}

impl KernelToolGateway {
    #[must_use]
    pub const fn new(kernel: Arc<Kernel>) -> Self {
        Self { kernel }
    }
}

#[async_trait]
impl AgentToolExecutor for KernelToolGateway {
    async fn execute(&self, tool_calls: &[ToolCallInfo]) -> Vec<ToolCallResult> {
        let proposals: Vec<aura_core_types::ToolProposal> = tool_calls
            .iter()
            .map(|tc| aura_core_types::ToolProposal::new(&tc.id, &tc.name, tc.input.clone()))
            .collect();

        match self.kernel.process_tools(proposals).await {
            Ok(results) => results
                .into_iter()
                .enumerate()
                .map(|(i, r)| {
                    if let Some(output) = r.tool_output {
                        let file_changes = if output.is_error {
                            Vec::new()
                        } else {
                            helpers::infer_file_changes(
                                &tool_calls[i].name,
                                &tool_calls[i].input,
                                None,
                                output.line_diff.as_ref(),
                            )
                        };
                        ToolCallResult {
                            tool_use_id: output.tool_use_id,
                            content: output.content,
                            is_error: output.is_error,
                            kind: output.kind,
                            stop_loop: false,
                            file_changes,
                            image: output.image,
                        }
                    } else {
                        let tc = &tool_calls[i];
                        ToolCallResult::error(&tc.id, "No output from kernel")
                    }
                })
                .collect(),
            Err(e) => {
                warn!(error = %e, "Kernel process_tools failed");
                tool_calls
                    .iter()
                    .map(|tc| ToolCallResult::error(&tc.id, format!("Kernel error: {e}")))
                    .collect()
            }
        }
    }
}

// ============================================================================
// KernelModelGateway
// ============================================================================

/// Routes [`ModelProvider`] calls through the kernel's reasoning layer,
/// ensuring all model interactions are recorded in the append-only log.
pub struct KernelModelGateway {
    kernel: Arc<Kernel>,
}

impl KernelModelGateway {
    #[must_use]
    pub const fn new(kernel: Arc<Kernel>) -> Self {
        Self { kernel }
    }
}

mod sealed {
    /// Crate-private seal: only types in `aura-agent` can implement
    /// [`super::RecordingModelProvider`]. External crates cannot satisfy
    /// this bound by hand-rolling a `ModelProvider` impl.
    pub trait Sealed {}
}

/// Marker trait for [`ModelProvider`] impls that route every model call
/// through `Kernel::reason` / `Kernel::reason_streaming` (Invariant §1
/// "Sole External Gateway" / §3 "Every LLM Call Is Recorded").
///
/// Sealed: external crates cannot implement this. The only public
/// implementation today is [`KernelModelGateway`], so any function that
/// requires `RecordingModelProvider` is structurally guaranteed to
/// receive a kernel-mediated provider — not a raw HTTP client.
///
/// Production-side automaton constructors in `aura-automaton`
/// (`DevLoopAutomaton::new`, `TaskRunAutomaton::new`,
/// `SpecGenAutomaton::new`, `ChatAutomaton::new`) take an
/// `Arc<P: RecordingModelProvider>` rather than `Arc<dyn ModelProvider>`
/// so the type system enforces this invariant.
pub trait RecordingModelProvider: ModelProvider + sealed::Sealed {}

impl sealed::Sealed for KernelModelGateway {}
impl RecordingModelProvider for KernelModelGateway {}

#[async_trait]
impl ModelProvider for KernelModelGateway {
    fn name(&self) -> &'static str {
        "kernel-gateway"
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, ReasonerError> {
        let result = self
            .kernel
            .reason(request)
            .await
            .map_err(kernel_error_to_reasoner_error)?;
        Ok(result.response)
    }

    async fn complete_streaming(
        &self,
        request: ModelRequest,
    ) -> Result<StreamEventStream, ReasonerError> {
        let (handle, stream) = self
            .kernel
            .reason_streaming(request)
            .await
            .map_err(kernel_error_to_reasoner_error)?;

        // Wrap the raw provider stream in a `RecordingStream` so the
        // `ReasonStreamHandle` is always finalized exactly once —
        // on natural end, error, or drop — per Invariant §3.
        let recording = RecordingStream::new(stream, handle);
        Ok(Box::pin(recording))
    }

    async fn health_check(&self) -> bool {
        true
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aura_agent_kernel::{ExecutorRouter, KernelConfig};
    use aura_core_types::AgentId;
    use aura_model_reasoner::{Message, MockProvider};
    use aura_store_db::RocksStore;
    use aura_tools::ToolExecutor;
    use serde_json::json;
    use tempfile::TempDir;

    fn create_test_kernel() -> (Arc<Kernel>, TempDir, TempDir) {
        let db_dir = TempDir::new().unwrap();
        let ws_dir = TempDir::new().unwrap();
        let agent_id = AgentId::generate();
        let store: Arc<dyn aura_store_db::Store> =
            Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("gateway test response"));
        let executor = ExecutorRouter::new();
        let config = KernelConfig {
            workspace_base: ws_dir.path().to_path_buf(),
            ..KernelConfig::default()
        };
        let kernel = Arc::new(Kernel::new(store, provider, executor, config, agent_id).unwrap());
        (kernel, db_dir, ws_dir)
    }

    fn create_tool_kernel(
        workspace_root: &std::path::Path,
        use_workspace_base_as_root: bool,
    ) -> (Arc<Kernel>, TempDir) {
        let db_dir = TempDir::new().unwrap();
        let agent_id = AgentId::generate();
        let store: Arc<dyn aura_store_db::Store> =
            Arc::new(RocksStore::open(db_dir.path(), false).unwrap());
        let provider: Arc<dyn ModelProvider + Send + Sync> =
            Arc::new(MockProvider::simple_response("gateway test response"));
        let mut executor = ExecutorRouter::new();
        executor.add_executor(Arc::new(ToolExecutor::with_defaults()));
        let config = KernelConfig {
            workspace_base: workspace_root.to_path_buf(),
            use_workspace_base_as_root,
            ..KernelConfig::default()
        };
        (
            Arc::new(Kernel::new(store, provider, executor, config, agent_id).unwrap()),
            db_dir,
        )
    }

    #[tokio::test]
    async fn test_model_gateway_complete() {
        let (kernel, _db, _ws) = create_test_kernel();
        let gateway = KernelModelGateway::new(kernel);

        let request = ModelRequest::builder("test-model", "system")
            .message(Message::user("hello"))
            .try_build()
            .unwrap();
        let response = gateway.complete(request).await.unwrap();
        assert!(!response.message.content.is_empty());
    }

    #[tokio::test]
    async fn test_model_gateway_name() {
        let (kernel, _db, _ws) = create_test_kernel();
        let gateway = KernelModelGateway::new(kernel);
        assert_eq!(gateway.name(), "kernel-gateway");
    }

    #[tokio::test]
    async fn test_tool_gateway_empty_batch() {
        let (kernel, _db, _ws) = create_test_kernel();
        let gateway = KernelToolGateway::new(kernel);
        let results = gateway.execute(&[]).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_tool_gateway_reads_from_workspace_root_when_configured() {
        let ws_dir = TempDir::new().unwrap();
        std::fs::write(
            ws_dir.path().join("reference.md"),
            "This file lives in the workspace root.",
        )
        .unwrap();

        let (kernel, _db_dir) = create_tool_kernel(ws_dir.path(), true);
        let gateway = KernelToolGateway::new(kernel);
        let results = gateway
            .execute(&[ToolCallInfo {
                id: "tool-1".into(),
                name: "read_file".into(),
                input: json!({ "path": "reference.md" }),
            }])
            .await;

        assert_eq!(results.len(), 1);
        assert!(
            !results[0].is_error,
            "tool output was {}",
            results[0].content
        );

        // The kernel decodes ToolResult.stdout/stderr from their
        // base64-on-the-wire form back to plain UTF-8 text before
        // handing content to the LLM, so the LLM no longer sees the
        // JSON-wrapped payload.
        assert_eq!(results[0].content, "This file lives in the workspace root.");
    }
}
