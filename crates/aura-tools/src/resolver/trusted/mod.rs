//! Trusted-integration runtime execution — GitHub, Linear, Slack,
//! Brave Search, Resend, and the generic `TrustedIntegrationRuntimeSpec`
//! interpreter.
//!
//! Phase 2b split this 1.1 KLoC module into a directory:
//!
//! - [`mod@http`] — unified `send_provider_request` plus the legacy
//!   `provider_json_request` / `provider_form_request` thin wrappers.
//! - [`mod@transforms`] — `apply_result_transform` (Brave, project-array,
//!   project-object, wrap-pointer) plus the shared `brave_search_results`
//!   helper used by both the inline `brave_search` integration and the
//!   declarative `BraveSearch` transform.
//! - [`mod@guards`] — `apply_success_guard` (Slack OK, GraphQL errors)
//!   plus the shared `graphql_user_errors` helper used by both the
//!   declarative `GraphqlErrors` guard and the bespoke `linear_graphql`
//!   integration.
//! - [`integrations`] — per-integration handlers (`github_*`, `linear_*`,
//!   `brave_search`, `slack_*`, `resend_*`).
//!
//! `mod.rs` keeps the `TrustedIntegrationRuntimeSpec` schema, the per-spec
//! dispatch entry-point (`execute_trusted_runtime_app_provider`), the
//! per-integration dispatch entry-point (`execute_runtime_app_provider`),
//! and the small free helpers shared across the submodules (binding
//! resolution, URL templating, runtime-spec parsing).

use super::json_paths::{
    insert_json_path, optional_boolean_from_names, optional_boolean_from_names_map,
    optional_json_from_names, optional_json_from_names_map, optional_positive_number_from_names,
    optional_positive_number_from_names_map, optional_string, optional_string_from_names,
    optional_string_from_names_map, optional_string_list_from_names,
    optional_string_list_from_names_map, required_string,
};
use super::{ToolResolver, TRUSTED_INTEGRATION_RUNTIME_METADATA_KEY};
use crate::error::ToolError;
use aura_core_types::{
    InstalledToolDefinition, InstalledToolRuntimeIntegration, InstalledToolRuntimeProviderExecution,
};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

mod guards;
mod http;
mod integrations;
mod transforms;

// ============================================================================
// Trusted integration runtime metadata schema
// ============================================================================

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TrustedIntegrationHttpMethod {
    Get,
    Post,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TrustedIntegrationArgValueType {
    String,
    StringList,
    PositiveNumber,
    Boolean,
    Json,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(super) enum TrustedIntegrationArgSource {
    #[default]
    InputArgs,
    ProviderConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrustedIntegrationArgBinding {
    pub(super) arg_names: Vec<String>,
    pub(super) target: String,
    #[serde(default)]
    pub(super) source: TrustedIntegrationArgSource,
    pub(super) value_type: TrustedIntegrationArgValueType,
    #[serde(default)]
    pub(super) required: bool,
    #[serde(default)]
    pub(super) default_value: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum TrustedIntegrationSuccessGuard {
    #[default]
    None,
    SlackOk,
    GraphqlErrors,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrustedIntegrationResultField {
    pub(super) output: String,
    pub(super) pointer: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TrustedIntegrationResultExtraField {
    pub(super) output: String,
    pub(super) pointer: String,
    #[serde(default)]
    pub(super) default_value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum TrustedIntegrationResultTransform {
    WrapPointer {
        key: String,
        pointer: String,
    },
    ProjectArray {
        key: String,
        #[serde(default)]
        pointer: Option<String>,
        fields: Vec<TrustedIntegrationResultField>,
        #[serde(default)]
        extras: Vec<TrustedIntegrationResultExtraField>,
    },
    ProjectObject {
        key: String,
        #[serde(default)]
        pointer: Option<String>,
        fields: Vec<TrustedIntegrationResultField>,
    },
    BraveSearch {
        vertical: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum TrustedIntegrationRuntimeSpec {
    RestJson {
        method: TrustedIntegrationHttpMethod,
        path: String,
        #[serde(default)]
        query: Vec<TrustedIntegrationArgBinding>,
        #[serde(default)]
        body: Vec<TrustedIntegrationArgBinding>,
        #[serde(default)]
        success_guard: TrustedIntegrationSuccessGuard,
        result: TrustedIntegrationResultTransform,
    },
    RestForm {
        method: TrustedIntegrationHttpMethod,
        path: String,
        #[serde(default)]
        query: Vec<TrustedIntegrationArgBinding>,
        #[serde(default)]
        body: Vec<TrustedIntegrationArgBinding>,
        #[serde(default)]
        success_guard: TrustedIntegrationSuccessGuard,
        result: TrustedIntegrationResultTransform,
    },
    Graphql {
        query: String,
        #[serde(default)]
        variables: Vec<TrustedIntegrationArgBinding>,
        #[serde(default)]
        success_guard: TrustedIntegrationSuccessGuard,
        result: TrustedIntegrationResultTransform,
    },
    BraveSearch {
        vertical: String,
    },
    ResendSendEmail,
    GmailSendEmail,
    GmailCreateDraft,
    GoogleCalendarCreateEvent,
    GoogleCalendarUpdateEvent,
    GoogleCalendarDeleteEvent,
}

// ============================================================================
// Dispatch entry points
// ============================================================================

impl ToolResolver {
    /// Spec-driven dispatch: interpret a [`TrustedIntegrationRuntimeSpec`]
    /// (sourced from `trusted_integration_runtime` metadata) and execute
    /// the corresponding HTTP / GraphQL / built-in integration call.
    pub(super) async fn execute_trusted_runtime_app_provider(
        &self,
        provider: &InstalledToolRuntimeProviderExecution,
        integration: &InstalledToolRuntimeIntegration,
        args: &Value,
        spec: &TrustedIntegrationRuntimeSpec,
    ) -> Result<Value, ToolError> {
        match spec {
            TrustedIntegrationRuntimeSpec::RestJson {
                method,
                path,
                query,
                body,
                success_guard,
                result,
            } => {
                let url = build_runtime_url(provider, integration, path, query, args)?;
                let body = build_object_from_bindings(body, args, &integration.provider_config)?;
                let response = self
                    .provider_json_request(
                        trusted_http_method(method),
                        &url,
                        provider,
                        integration,
                        body,
                    )
                    .await?;
                guards::apply_success_guard(&response, success_guard)?;
                transforms::apply_result_transform(&response, result, args)
            }
            TrustedIntegrationRuntimeSpec::Graphql {
                query,
                variables,
                success_guard,
                result,
            } => {
                let variables =
                    build_object_from_bindings(variables, args, &integration.provider_config)?
                        .unwrap_or_else(|| json!({}));
                let response = self
                    .provider_json_request(
                        Method::POST,
                        &provider.base_url,
                        provider,
                        integration,
                        Some(json!({
                            "query": query,
                            "variables": variables,
                        })),
                    )
                    .await?;
                guards::apply_success_guard(&response, success_guard)?;
                transforms::apply_result_transform(&response, result, args)
            }
            TrustedIntegrationRuntimeSpec::RestForm {
                method,
                path,
                query,
                body,
                success_guard,
                result,
            } => {
                let url = build_runtime_url(provider, integration, path, query, args)?;
                let body =
                    build_form_fields_from_bindings(body, args, &integration.provider_config)?;
                let response = self
                    .provider_form_request(
                        trusted_http_method(method),
                        &url,
                        provider,
                        integration,
                        body,
                    )
                    .await?;
                guards::apply_success_guard(&response, success_guard)?;
                transforms::apply_result_transform(&response, result, args)
            }
            TrustedIntegrationRuntimeSpec::BraveSearch { vertical } => {
                self.brave_search(provider, integration, args, vertical)
                    .await
            }
            TrustedIntegrationRuntimeSpec::ResendSendEmail => {
                self.resend_send_email(provider, integration, args).await
            }
            TrustedIntegrationRuntimeSpec::GmailSendEmail => {
                self.gmail_send_email(provider, integration, args).await
            }
            TrustedIntegrationRuntimeSpec::GmailCreateDraft => {
                self.gmail_create_draft(provider, integration, args).await
            }
            TrustedIntegrationRuntimeSpec::GoogleCalendarCreateEvent => {
                self.google_calendar_create_event(provider, integration, args)
                    .await
            }
            TrustedIntegrationRuntimeSpec::GoogleCalendarUpdateEvent => {
                self.google_calendar_update_event(provider, integration, args)
                    .await
            }
            TrustedIntegrationRuntimeSpec::GoogleCalendarDeleteEvent => {
                self.google_calendar_delete_event(provider, integration, args)
                    .await
            }
        }
    }

    /// Per-integration dispatch: pick a hard-coded handler by tool name
    /// for the providers Aura ships with built-in support.
    pub(super) async fn execute_runtime_app_provider(
        &self,
        tool: &InstalledToolDefinition,
        args: &Value,
        provider: &InstalledToolRuntimeProviderExecution,
    ) -> Result<Value, ToolError> {
        let integration = select_runtime_integration(provider, args)?;
        match tool.name.as_str() {
            "github_list_repos" => self.github_list_repos(provider, integration).await,
            "github_create_issue" => self.github_create_issue(provider, integration, args).await,
            "linear_list_teams" => self.linear_list_teams(provider, integration).await,
            "linear_create_issue" => self.linear_create_issue(provider, integration, args).await,
            "slack_list_channels" => self.slack_list_channels(provider, integration).await,
            "slack_post_message" => self.slack_post_message(provider, integration, args).await,
            "brave_search_web" => self.brave_search(provider, integration, args, "web").await,
            "brave_search_news" => self.brave_search(provider, integration, args, "news").await,
            "resend_list_domains" => self.resend_list_domains(provider, integration).await,
            "resend_send_email" => self.resend_send_email(provider, integration, args).await,
            other => Err(ToolError::ExternalToolError(format!(
                "runtime execution is not implemented for installed tool `{other}`"
            ))),
        }
    }
}

// ============================================================================
// Shared helpers — runtime spec parsing, integration selection, binding
// resolution, URL templating
// ============================================================================

/// Parse the `trusted_integration_runtime` metadata blob (if any) on
/// `tool` into a typed [`TrustedIntegrationRuntimeSpec`]. Returns
/// `Ok(None)` when the tool has no such metadata.
pub(super) fn trusted_runtime_spec(
    tool: &InstalledToolDefinition,
) -> Result<Option<TrustedIntegrationRuntimeSpec>, ToolError> {
    let Some(raw) = tool.metadata.get(TRUSTED_INTEGRATION_RUNTIME_METADATA_KEY) else {
        return Ok(None);
    };
    serde_json::from_value(raw.clone()).map(Some).map_err(|e| {
        ToolError::ExternalToolError(format!(
            "invalid trusted integration runtime metadata for `{}`: {e}",
            tool.name
        ))
    })
}

/// Resolve an `integration_id` from `args` (or default to the first) to
/// pick which credential set the dispatch should use.
pub(super) fn select_runtime_integration<'a>(
    provider: &'a InstalledToolRuntimeProviderExecution,
    args: &Value,
) -> Result<&'a InstalledToolRuntimeIntegration, ToolError> {
    let requested = optional_string(args, &["integration_id", "integrationId"]);
    if let Some(requested) = requested {
        return provider
            .integrations
            .iter()
            .find(|integration| integration.integration_id == requested)
            .ok_or_else(|| {
                ToolError::ExternalToolError(format!(
                    "requested integration `{requested}` is not installed for runtime execution"
                ))
            });
    }
    provider.integrations.first().ok_or_else(|| {
        ToolError::ExternalToolError("no runtime integration credentials are available".into())
    })
}

pub(super) fn trusted_http_method(method: &TrustedIntegrationHttpMethod) -> Method {
    match method {
        TrustedIntegrationHttpMethod::Get => Method::GET,
        TrustedIntegrationHttpMethod::Post => Method::POST,
    }
}

pub(super) fn build_runtime_url(
    provider: &InstalledToolRuntimeProviderExecution,
    integration: &InstalledToolRuntimeIntegration,
    path: &str,
    query_bindings: &[TrustedIntegrationArgBinding],
    args: &Value,
) -> Result<String, ToolError> {
    let expanded_path = expand_path_template(path, args)?;
    let resolved_base_url = integration
        .base_url
        .as_deref()
        .unwrap_or(&provider.base_url);
    let base = format!(
        "{}{}",
        resolved_base_url.trim_end_matches('/'),
        expanded_path
    );
    let mut url = Url::parse(&base)
        .map_err(|e| ToolError::ExternalToolError(format!("invalid trusted runtime url: {e}")))?;
    for binding in query_bindings {
        if let Some(value) = resolve_binding_value(args, &integration.provider_config, binding)? {
            append_query_value(&mut url, &binding.target, value);
        }
    }
    Ok(url.to_string())
}

fn expand_path_template(path: &str, args: &Value) -> Result<String, ToolError> {
    let mut expanded = String::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut key = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                key.push(next);
            }
            let value = required_string(args, &[key.as_str()])?;
            expanded.push_str(&value);
        } else {
            expanded.push(ch);
        }
    }
    Ok(expanded)
}

fn append_query_value(url: &mut Url, key: &str, value: Value) {
    let mut pairs = url.query_pairs_mut();
    match value {
        Value::Array(items) => {
            for item in items {
                if let Some(value) = item.as_str() {
                    pairs.append_pair(key, value);
                } else {
                    pairs.append_pair(key, &item.to_string());
                }
            }
        }
        Value::String(value) => {
            pairs.append_pair(key, &value);
        }
        other => {
            pairs.append_pair(key, &other.to_string());
        }
    }
}

pub(super) fn build_object_from_bindings(
    bindings: &[TrustedIntegrationArgBinding],
    args: &Value,
    provider_config: &HashMap<String, Value>,
) -> Result<Option<Value>, ToolError> {
    if bindings.is_empty() {
        return Ok(None);
    }

    if bindings.len() == 1 && bindings[0].target == "$" {
        return resolve_binding_value(args, provider_config, &bindings[0]);
    }

    let mut body = json!({});
    let mut inserted = false;
    for binding in bindings {
        if binding.target == "$" {
            return Err(ToolError::ExternalToolError(
                "trusted integration metadata cannot mix root body bindings with object bindings"
                    .into(),
            ));
        }
        if let Some(value) = resolve_binding_value(args, provider_config, binding)? {
            insert_json_path(&mut body, &binding.target, value)?;
            inserted = true;
        }
    }
    Ok(inserted.then_some(body))
}

pub(super) fn build_form_fields_from_bindings(
    bindings: &[TrustedIntegrationArgBinding],
    args: &Value,
    provider_config: &HashMap<String, Value>,
) -> Result<Vec<(String, String)>, ToolError> {
    let mut fields = Vec::new();
    for binding in bindings {
        if let Some(value) = resolve_binding_value(args, provider_config, binding)? {
            match value {
                Value::Array(items) => {
                    for item in items {
                        fields.push((binding.target.clone(), form_field_value(item)));
                    }
                }
                other => fields.push((binding.target.clone(), form_field_value(other))),
            }
        }
    }
    Ok(fields)
}

fn form_field_value(value: Value) -> String {
    match value {
        Value::String(value) => value,
        other => other.to_string(),
    }
}

fn resolve_binding_value(
    args: &Value,
    provider_config: &HashMap<String, Value>,
    binding: &TrustedIntegrationArgBinding,
) -> Result<Option<Value>, ToolError> {
    if binding.arg_names.is_empty() {
        return Ok(binding.default_value.clone());
    }

    let resolved = match binding.source {
        TrustedIntegrationArgSource::InputArgs => match binding.value_type {
            TrustedIntegrationArgValueType::String => {
                optional_string_from_names(args, &binding.arg_names).map(Value::String)
            }
            TrustedIntegrationArgValueType::StringList => {
                optional_string_list_from_names(args, &binding.arg_names).map(|items| json!(items))
            }
            TrustedIntegrationArgValueType::PositiveNumber => {
                optional_positive_number_from_names(args, &binding.arg_names)
                    .map(|value| json!(value))
            }
            TrustedIntegrationArgValueType::Boolean => {
                optional_boolean_from_names(args, &binding.arg_names).map(|value| json!(value))
            }
            TrustedIntegrationArgValueType::Json => {
                optional_json_from_names(args, &binding.arg_names)
            }
        },
        TrustedIntegrationArgSource::ProviderConfig => match binding.value_type {
            TrustedIntegrationArgValueType::String => {
                optional_string_from_names_map(provider_config, &binding.arg_names)
                    .map(Value::String)
            }
            TrustedIntegrationArgValueType::StringList => {
                optional_string_list_from_names_map(provider_config, &binding.arg_names)
                    .map(|items| json!(items))
            }
            TrustedIntegrationArgValueType::PositiveNumber => {
                optional_positive_number_from_names_map(provider_config, &binding.arg_names)
                    .map(|value| json!(value))
            }
            TrustedIntegrationArgValueType::Boolean => {
                optional_boolean_from_names_map(provider_config, &binding.arg_names)
                    .map(|value| json!(value))
            }
            TrustedIntegrationArgValueType::Json => {
                optional_json_from_names_map(provider_config, &binding.arg_names)
            }
        },
    };

    if let Some(value) = resolved {
        return Ok(Some(value));
    }
    if let Some(default) = &binding.default_value {
        return Ok(Some(default.clone()));
    }
    if binding.required {
        return Err(ToolError::ExternalToolError(format!(
            "missing required field `{}`",
            binding.arg_names.first().map_or("", String::as_str)
        )));
    }
    Ok(None)
}
