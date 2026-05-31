//! Installed-tool HTTP dispatch — both the plain POST-to-endpoint path
//! and the runtime-execution fan-out that delegates to the trusted
//! provider layer.
//!
//! Split out of `resolver.rs` in Wave 6 / T4.

use super::trusted::{select_runtime_integration, trusted_runtime_spec};
use super::ToolResolver;
use crate::error::ToolError;
use aura_core::{
    InstalledToolDefinition, InstalledToolRuntimeExecution, ToolAuth, ToolCall, ToolResult,
};
use aura_exec_traits::ExecuteContext;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

impl ToolResolver {
    /// Execute a tool call:
    /// 1. Installed tool with HTTP endpoint (or runtime execution).
    /// 2. Domain executor when attached (pure HTTP — no sandbox needed).
    /// 3. Delegate to the inner [`ToolExecutor`](crate::ToolExecutor) for
    ///    built-in tools.
    // `tool_call` is in the skip list so credential-bearing `args`
    // (e.g. `jwt` on `git_commit_push`) never land in tracing spans.
    // The tool name is surfaced via `fields(tool = ...)`; `ToolCall`
    // also has a redacting `Debug` impl as defense in depth.
    #[tracing::instrument(skip(self, ctx, tool_call), fields(tool = %tool_call.tool))]
    pub(super) async fn execute_tool(
        &self,
        ctx: &ExecuteContext,
        tool_call: &ToolCall,
    ) -> Result<ToolResult, ToolError> {
        let tool_name = &tool_call.tool;

        if let Some(tool) = self.installed_tools.get(tool_name) {
            return self
                .execute_installed_tool(ctx, tool, &tool_call.args)
                .await;
        }

        // Domain tools (specs, tasks, project) — pure HTTP calls that
        // never touch the filesystem, so they must be dispatched before
        // Sandbox::new to avoid failing when the workspace dir is
        // inaccessible (e.g. remote agent on a different OS).
        if let Some(ref domain) = self.domain_executor {
            if domain.handles(tool_name) {
                let project_id = tool_call.args["project_id"].as_str().unwrap_or_default();
                let result_json = domain.execute(tool_name, project_id, &tool_call.args).await;
                let is_error = serde_json::from_str::<serde_json::Value>(&result_json)
                    .ok()
                    .and_then(|v| v.get("ok")?.as_bool())
                    .is_some_and(|ok| !ok);
                if is_error {
                    return Ok(ToolResult::failure(tool_name, result_json));
                }
                return Ok(ToolResult::success(tool_name, result_json));
            }
        }

        // Built-in tools — delegates permission checks, sandbox, and dispatch
        // to ToolExecutor so the logic is not duplicated.
        self.inner.execute_tool(ctx, tool_call).await
    }

    async fn execute_installed_tool(
        &self,
        ctx: &ExecuteContext,
        tool: &InstalledToolDefinition,
        args: &Value,
    ) -> Result<ToolResult, ToolError> {
        if let Some(runtime_execution) = &tool.runtime_execution {
            return self
                .execute_runtime_installed_tool(ctx, tool, args, runtime_execution)
                .await;
        }

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match &tool.auth {
            ToolAuth::None => {}
            ToolAuth::Bearer { token } => {
                let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| {
                    ToolError::ExternalToolError(format!("invalid bearer auth header: {e}"))
                })?;
                headers.insert(AUTHORIZATION, value);
            }
            ToolAuth::ApiKey { header, key } => {
                let name = HeaderName::from_bytes(header.as_bytes()).map_err(|e| {
                    ToolError::ExternalToolError(format!("invalid auth header name: {e}"))
                })?;
                let value = HeaderValue::from_str(key).map_err(|e| {
                    ToolError::ExternalToolError(format!("invalid api key header value: {e}"))
                })?;
                headers.insert(name, value);
            }
            ToolAuth::Headers { headers: extra } => {
                for (name, value) in extra {
                    let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                        ToolError::ExternalToolError(format!(
                            "invalid auth header name `{name}`: {e}"
                        ))
                    })?;
                    let header_value = HeaderValue::from_str(value).map_err(|e| {
                        ToolError::ExternalToolError(format!(
                            "invalid auth header value for `{name}`: {e}"
                        ))
                    })?;
                    headers.insert(header_name, header_value);
                }
            }
        }

        headers.insert(
            HeaderName::from_static("x-aura-agent-id"),
            HeaderValue::from_str(&ctx.agent_id.to_string()).map_err(|e| {
                ToolError::ExternalToolError(format!("invalid x-aura-agent-id header: {e}"))
            })?,
        );

        let request = self
            .http_client
            .post(&tool.endpoint)
            .headers(headers)
            .json(args)
            .timeout(std::time::Duration::from_millis(
                tool.timeout_ms.unwrap_or(30_000),
            ));

        let response =
            request
                .send()
                .await
                .map_err(|e| ToolError::ExternalToolCallbackUnreachable {
                    url: tool.endpoint.clone(),
                    reason: e.to_string(),
                })?;
        let status = response.status();
        let body = response.text().await.map_err(|e| {
            ToolError::ExternalToolError(format!("reading installed tool response failed: {e}"))
        })?;

        if status.is_success() {
            Ok(ToolResult::success(&tool.name, body))
        } else {
            Err(ToolError::ExternalToolCallbackFailed {
                url: tool.endpoint.clone(),
                status: status.as_u16(),
                body,
            })
        }
    }

    async fn execute_runtime_installed_tool(
        &self,
        _ctx: &ExecuteContext,
        tool: &InstalledToolDefinition,
        args: &Value,
        execution: &InstalledToolRuntimeExecution,
    ) -> Result<ToolResult, ToolError> {
        let result = match execution {
            InstalledToolRuntimeExecution::AppProvider(provider) => {
                if let Some(spec) = trusted_runtime_spec(tool)? {
                    let integration = select_runtime_integration(provider, args)?;
                    self.execute_trusted_runtime_app_provider(provider, integration, args, &spec)
                        .await?
                } else {
                    self.execute_runtime_app_provider(tool, args, provider)
                        .await?
                }
            }
        };
        Ok(ToolResult::success(&tool.name, result.to_string()))
    }
}
