//! Domain tool handlers and the `DomainApi` callback trait.
//!
//! This module provides the harness-side tool handlers for domain operations
//! (specs, tasks, projects, orbit, network, storage). The actual data access
//! is delegated through the [`DomainApi`] trait so that the harness never
//! depends on app-level crates.

pub mod api;
pub mod helpers;
pub mod network;
pub mod orbit;
pub mod project;
pub mod specs;
pub mod storage;
pub mod tasks;

pub use api::*;

use serde_json::Value;
use std::sync::Arc;
use tracing::warn;

use helpers::domain_err;

// ---------------------------------------------------------------------------
// Why this list lives here (and is NOT shipped over the wire from the server)
// ---------------------------------------------------------------------------
//
// An earlier "unify on DomainApi" plan proposed adding a
// `domain_tools: Vec<String>` field to the run-start protocol so
// aura-os-server could advertise the domain-tool set and the harness would
// treat that wire-shipped list as the source of truth instead of this slice.
// That change was evaluated and intentionally skipped. The reasoning, left
// here so the next person doesn't resurrect the todo:
//
// 1. `DOMAIN_TOOL_NAMES` and the `match tool_name` dispatch inside
//    `DomainToolExecutor::execute` (see below) are co-located in this
//    file by design. Adding a name without a matching handler arm (or
//    vice versa) is a single-file review diff, not a cross-crate hunt.
//    The handler file IS the source of truth by construction.
//
// 2. `aura-os-server` has no `impl DomainApi` of its own — the only
//    implementation is `HttpDomainApi` in `crates/aura-runtime/src/domain.rs`.
//    If the server were to ship a `domain_tools` list over the wire it
//    would have to duplicate this slice (or fetch it from the harness it
//    is calling back into), moving the source of truth to a strictly
//    worse place and introducing a drift risk between the server's
//    copy and the harness's dispatch.
//
// 3. The effective catalog and resolver already get the full set from the
//    live `DomainToolExecutor`, which is derived from this same slice via
//    `tool_names()`. That is exactly the behavior a wire-shipped
//    run-start `domain_tools` field would have produced — with no wire
//    coupling.
//
// If you ever need the server to advertise this list for *diagnostic*
// purposes (e.g. a debug endpoint that shows "what can this harness
// handle?"), drive it from the harness via `SessionReady` rather than
// reintroducing it on `RuntimeRequest`. See `DomainToolExecutor::tool_names`.
const DOMAIN_TOOL_NAMES: &[&str] = &[
    "list_specs",
    "get_spec",
    "create_spec",
    "update_spec",
    "update_spec_section",
    "append_to_spec",
    "delete_spec",
    "list_tasks",
    "get_task",
    "create_task",
    "update_task",
    "delete_task",
    "transition_task",
    "get_project",
    "update_project",
    "assign_agent_to_project",
    "create_log",
    "list_logs",
    "get_project_stats",
    "orbit_push",
    "orbit_create_repo",
    "orbit_list_repos",
    "orbit_list_branches",
    "orbit_create_branch",
    "orbit_list_commits",
    "orbit_get_diff",
    "orbit_create_pr",
    "orbit_list_prs",
    "orbit_merge_pr",
    "post_to_feed",
    "list_projects",
    "check_budget",
    "record_usage",
];

/// Dispatches domain tool calls to the appropriate handler via `DomainApi`.
pub struct DomainToolExecutor {
    api: Arc<dyn DomainApi>,
    /// Per-session JWT for orbit/network calls that need user auth.
    session_jwt: Option<String>,
    /// Per-session project ID; used as fallback when the LLM tool call args
    /// do not include `project_id` (single-project mode).
    session_project_id: Option<String>,
    /// Per-session workspace path; injected into orbit tool calls that need
    /// a local working directory (e.g. `orbit_push`).
    session_workspace: Option<String>,
}

impl DomainToolExecutor {
    pub fn new(api: Arc<dyn DomainApi>) -> Self {
        Self {
            api,
            session_jwt: None,
            session_project_id: None,
            session_workspace: None,
        }
    }

    /// Create an executor with a session-scoped JWT for orbit/network auth.
    pub fn with_session_jwt(api: Arc<dyn DomainApi>, jwt: Option<String>) -> Self {
        Self {
            api,
            session_jwt: jwt,
            session_project_id: None,
            session_workspace: None,
        }
    }

    /// Create an executor with both session JWT and project ID context.
    ///
    /// In single-project mode the LLM tool schemas omit `project_id`, so
    /// the session-level value is used as a fallback.
    pub fn with_session_context(
        api: Arc<dyn DomainApi>,
        jwt: Option<String>,
        project_id: Option<String>,
        workspace: Option<String>,
    ) -> Self {
        Self {
            api,
            session_jwt: jwt,
            session_project_id: project_id,
            session_workspace: workspace,
        }
    }

    /// Inject session-scoped fields (JWT, workspace) into input args so
    /// orbit/network handlers can use them without the LLM needing to provide them.
    fn inject_session_fields(&self, input: &Value) -> Value {
        let mut patched = input.clone();
        if let Some(obj) = patched.as_object_mut() {
            if let Some(jwt) = &self.session_jwt {
                if !obj.contains_key("jwt") {
                    obj.insert("jwt".to_string(), Value::String(jwt.clone()));
                }
            }
            if let Some(workspace) = &self.session_workspace {
                if !obj.contains_key("workspace") {
                    obj.insert("workspace".to_string(), Value::String(workspace.clone()));
                }
            }
        }
        patched
    }

    /// Resolve the effective project ID: prefer the explicit arg from the tool
    /// call, fall back to the session-level ID.
    fn effective_project_id<'a>(&'a self, from_args: &'a str) -> &'a str {
        if from_args.is_empty() {
            self.session_project_id.as_deref().unwrap_or_default()
        } else {
            from_args
        }
    }

    /// Execute a domain tool by name.
    ///
    /// `project_id` is threaded through from the resolver (extracted from tool
    /// call args).  When empty, the session-level project ID is used instead.
    /// The session JWT is injected into `input` so handlers can forward it.
    /// Returns a JSON string result (always contains an `ok` field).
    pub async fn execute(&self, tool_name: &str, project_id: &str, input: &Value) -> String {
        let project_id = self.effective_project_id(project_id);
        let input = self.inject_session_fields(input);
        match tool_name {
            // Specs
            "list_specs" => specs::list_specs(self.api.as_ref(), project_id, &input).await,
            "get_spec" => specs::get_spec(self.api.as_ref(), project_id, &input).await,
            "create_spec" => specs::create_spec(self.api.as_ref(), project_id, &input).await,
            "update_spec" => specs::update_spec(self.api.as_ref(), project_id, &input).await,
            "update_spec_section" => {
                specs::update_spec_section(self.api.as_ref(), project_id, &input).await
            }
            "append_to_spec" => specs::append_to_spec(self.api.as_ref(), project_id, &input).await,
            "delete_spec" => specs::delete_spec(self.api.as_ref(), project_id, &input).await,

            // Tasks
            "list_tasks" => tasks::list_tasks(self.api.as_ref(), project_id, &input).await,
            "get_task" => tasks::get_task(self.api.as_ref(), project_id, &input).await,
            "create_task" => tasks::create_task(self.api.as_ref(), project_id, &input).await,
            "update_task" => tasks::update_task(self.api.as_ref(), project_id, &input).await,
            "delete_task" => tasks::delete_task(self.api.as_ref(), project_id, &input).await,
            "transition_task" => {
                tasks::transition_task(self.api.as_ref(), project_id, &input).await
            }

            // Project
            "get_project" => project::get_project(self.api.as_ref(), project_id, &input).await,
            "update_project" => {
                project::update_project(self.api.as_ref(), project_id, &input).await
            }
            "assign_agent_to_project" => {
                project::assign_agent_to_project(self.api.as_ref(), project_id, &input).await
            }

            // Storage (logs, stats)
            "create_log" => storage::create_log(self.api.as_ref(), project_id, &input).await,
            "list_logs" => storage::list_logs(self.api.as_ref(), project_id, &input).await,
            "get_project_stats" => {
                storage::get_project_stats(self.api.as_ref(), project_id, &input).await
            }

            // Orbit (git operations)
            "orbit_push" => orbit::orbit_push(self.api.as_ref(), project_id, &input).await,
            "orbit_create_repo" => {
                orbit::orbit_create_repo(self.api.as_ref(), project_id, &input).await
            }
            "orbit_list_repos" => {
                orbit::orbit_list_repos(self.api.as_ref(), project_id, &input).await
            }
            "orbit_list_branches" => {
                orbit::orbit_list_branches(self.api.as_ref(), project_id, &input).await
            }
            "orbit_create_branch" => {
                orbit::orbit_create_branch(self.api.as_ref(), project_id, &input).await
            }
            "orbit_list_commits" => {
                orbit::orbit_list_commits(self.api.as_ref(), project_id, &input).await
            }
            "orbit_get_diff" => orbit::orbit_get_diff(self.api.as_ref(), project_id, &input).await,
            "orbit_create_pr" => {
                orbit::orbit_create_pr(self.api.as_ref(), project_id, &input).await
            }
            "orbit_list_prs" => orbit::orbit_list_prs(self.api.as_ref(), project_id, &input).await,
            "orbit_merge_pr" => orbit::orbit_merge_pr(self.api.as_ref(), project_id, &input).await,

            // Network (social, billing)
            "post_to_feed" => network::post_to_feed(self.api.as_ref(), project_id, &input).await,
            "list_projects" => {
                network::network_list_projects(self.api.as_ref(), project_id, &input).await
            }
            "check_budget" => network::check_budget(self.api.as_ref(), project_id, &input).await,
            "record_usage" => network::record_usage(self.api.as_ref(), project_id, &input).await,

            other => {
                warn!(tool = other, "unknown domain tool");
                domain_err(format!("unknown domain tool: {other}"))
            }
        }
    }

    /// Returns true if `tool_name` is a domain tool handled by this executor.
    pub fn handles(&self, tool_name: &str) -> bool {
        DOMAIN_TOOL_NAMES.contains(&tool_name)
    }

    /// List all domain tool names handled by this executor.
    pub fn tool_names(&self) -> &[&'static str] {
        DOMAIN_TOOL_NAMES
    }
}

/// Returns `true` if `name` is a recognised domain tool, regardless of
/// whether a [`DomainToolExecutor`] is currently wired up.
pub fn is_domain_tool(name: &str) -> bool {
    DOMAIN_TOOL_NAMES.contains(&name)
}
