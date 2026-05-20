//! Tool schema definitions for chat, engine, and multi-project agents.
//!
//! # Schema sourcing (Phase 1 — dedup)
//!
//! Core filesystem/shell/search tools have a corresponding `impl Tool`
//! in this crate (see `fs_tools/` and friends) with its own
//! [`Tool::definition`](crate::tool::Tool::definition) method. Those
//! `Tool::definition()` impls are the **single source of truth** for the
//! model-facing schema; [`core_tool_definitions`] simply collects them
//! from [`builtin_tools`] instead of re-stating each schema inline.
//!
//! Domain-management tools (`spec_*`, `task_*`, `orbit_*`, `network_*`,
//! `dev_loop_*`, `engine_specific_*`) have no `Tool` impl in this
//! crate — their execution is routed through the `DomainApi` /
//! automaton surface — so their schemas stay inline in this module
//! with a `TODO(phase-6): collapse when we confirm no external const
//! consumers` comment on the relevant groups.

use crate::tool::builtin_tools;
use aura_core_types::ToolDefinition;

// ============================================================================
// Helpers
// ============================================================================

/// Build an inline tool definition for the domain-management surface
/// (specs, tasks, projects, engine tools). Used only by entries that
/// don't have a `Tool` impl in this crate.
fn tool(name: &str, description: &str, schema: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
        cache_control: None,
        eager_input_streaming: eager_input_streaming_for(name),
    }
}

/// Build a tool definition with property-level descriptions stripped from the
/// JSON schema. Keeps property names, types, enums, required, and nested
/// structure — only removes the verbose "description" field on each property.
fn compact_tool(name: &str, description: &str, schema: serde_json::Value) -> ToolDefinition {
    ToolDefinition {
        name: name.into(),
        description: description.into(),
        input_schema: strip_property_descriptions(schema),
        cache_control: None,
        eager_input_streaming: eager_input_streaming_for(name),
    }
}

/// Tools whose arguments the UI wants to stream live into its preview card
/// (markdown spec body, file contents, diff text). Opting them into
/// Anthropic's fine-grained tool streaming makes `input_json_delta` events
/// arrive as raw partial string bytes while the model writes, instead of
/// being buffered until the full JSON validates at `content_block_stop`.
///
/// This is only used for tools built inline in this module (no `Tool`
/// impl). Built-in tools set this flag directly in their
/// [`Tool::definition`](crate::tool::Tool::definition).
fn eager_input_streaming_for(name: &str) -> Option<bool> {
    match name {
        "create_spec" | "update_spec" | "update_spec_section" | "append_to_spec" => Some(true),
        _ => None,
    }
}

fn strip_property_descriptions(mut schema: serde_json::Value) -> serde_json::Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for (_key, prop_val) in props.iter_mut() {
            if let Some(obj) = prop_val.as_object_mut() {
                obj.remove("description");
            }
        }
    }
    schema
}

/// Names of the built-in tools that form the "core" (filesystem/shell/search)
/// bundle advertised to every profile.
///
/// Kept separate from the `stat_file` + git tools, which are also built-in
/// but not part of the historical `core_tool_definitions()` surface.
/// Order matches the inline order used before Phase 1 — stable for any
/// snapshot tests.
const CORE_BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "delete_file",
    "list_files",
    "run_command",
    "search_code",
    "find_files",
];

/// Collect `Tool::definition()` for each name in `names`, preserving the
/// order of `names`. Panics in test builds if a name is missing — this
/// is strictly a refactor-integrity invariant (the hard-coded list in
/// this module must stay in sync with [`builtin_tools`]), and every
/// known caller constructs the name list at compile time.
fn definitions_for_builtin_names(names: &[&str]) -> Vec<ToolDefinition> {
    let builtins = builtin_tools();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        if let Some(tool) = builtins.iter().find(|t| t.name() == *name) {
            out.push(tool.definition());
        } else {
            debug_assert!(false, "builtin tool '{name}' missing from builtin_tools()");
        }
    }
    out
}

// ============================================================================
// Core tools (filesystem, shell, search)
// ============================================================================

/// Core tool schemas: filesystem, shell, search. Built from
/// [`Tool::definition`](crate::tool::Tool::definition) so any schema
/// change in a built-in tool's impl is the authoritative update.
pub fn core_tool_definitions() -> Vec<ToolDefinition> {
    definitions_for_builtin_names(CORE_BUILTIN_TOOL_NAMES)
}

/// File I/O subset of [`core_tool_definitions`]. Retained for
/// test-targeted assertions on the streaming flag.
#[cfg(test)]
fn file_io_tools() -> Vec<ToolDefinition> {
    definitions_for_builtin_names(&["read_file", "write_file", "edit_file"])
}

// ============================================================================
// Chat agent tools
// ============================================================================

pub fn chat_management_tools() -> Vec<ToolDefinition> {
    let mut tools = spec_tool_definitions();
    tools.extend(task_tool_definitions());
    tools.extend(project_tool_definitions());
    tools.extend(dev_loop_tool_definitions());
    tools.extend(orbit_tool_definitions());
    tools.extend(network_tool_definitions());
    tools
}

pub fn orbit_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool(
            "orbit_push",
            "Push a branch from the workspace to an orbit git repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"branch":{"type":"string"},"force":{"type":"boolean"}},"required":["org_id","repo","branch"]}),
        ),
        compact_tool(
            "orbit_create_repo",
            "Create a new orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"project_id":{"type":"string"},"owner_id":{"type":"string"},"name":{"type":"string"},"visibility":{"type":"string"}},"required":["org_id","project_id","owner_id","name"]}),
        ),
        compact_tool(
            "orbit_list_repos",
            "List accessible orbit repositories.",
            serde_json::json!({"type":"object","properties":{},"required":[]}),
        ),
        compact_tool(
            "orbit_list_branches",
            "List branches in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"}},"required":["org_id","repo"]}),
        ),
        compact_tool(
            "orbit_create_branch",
            "Create a branch in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"name":{"type":"string"},"source":{"type":"string"}},"required":["org_id","repo","name"]}),
        ),
        compact_tool(
            "orbit_list_commits",
            "List recent commits in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"ref":{"type":"string"},"limit":{"type":"integer"}},"required":["org_id","repo"]}),
        ),
        compact_tool(
            "orbit_get_diff",
            "Get diff for a commit in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"sha":{"type":"string"}},"required":["org_id","repo","sha"]}),
        ),
        compact_tool(
            "orbit_create_pr",
            "Open a pull request in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"source_branch":{"type":"string"},"target_branch":{"type":"string"},"title":{"type":"string"},"description":{"type":"string"}},"required":["org_id","repo","source_branch","target_branch","title"]}),
        ),
        compact_tool(
            "orbit_list_prs",
            "List pull requests in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"status":{"type":"string"}},"required":["org_id","repo"]}),
        ),
        compact_tool(
            "orbit_merge_pr",
            "Merge a pull request in an orbit repository.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"repo":{"type":"string"},"pr_id":{"type":"string"},"strategy":{"type":"string"}},"required":["org_id","repo","pr_id"]}),
        ),
    ]
}

pub fn network_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool(
            "post_to_feed",
            "Post a status update to the activity feed.",
            serde_json::json!({"type":"object","properties":{"profile_id":{"type":"string"},"title":{"type":"string"},"summary":{"type":"string"},"post_type":{"type":"string"},"agent_id":{"type":"string"},"user_id":{"type":"string"},"metadata":{"type":"object"}},"required":["profile_id","title"]}),
        ),
        compact_tool(
            "list_projects",
            "List projects in an organization.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"}},"required":["org_id"]}),
        ),
        compact_tool(
            "check_budget",
            "Check remaining credit budget for a user.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"user_id":{"type":"string"}},"required":["org_id","user_id"]}),
        ),
        compact_tool(
            "record_usage",
            "Record token usage for billing.",
            serde_json::json!({"type":"object","properties":{"org_id":{"type":"string"},"user_id":{"type":"string"},"input_tokens":{"type":"integer"},"output_tokens":{"type":"integer"},"agent_id":{"type":"string"},"model":{"type":"string"}},"required":["org_id","user_id","input_tokens","output_tokens"]}),
        ),
    ]
}

fn spec_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool("list_specs", "List specs in the current project. Returns metadata only (spec_id, title, order, markdown_bytes); use get_spec to fetch the body of a specific spec.", serde_json::json!({"type":"object","properties":{},"required":[]})),
        compact_tool("get_spec", "Fetch a single spec by its UUID spec_id (from list_specs or create_spec output, NOT the title number). The returned markdown_contents is capped at 64 KB; oversize specs are truncated with a marker and accompanied by truncated_markdown=true and total_markdown_bytes. For large specs, use update_spec rather than re-reading the body each turn.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string","description":"The spec ID"}},"required":["spec_id"]})),
        compact_tool("create_spec", "Create a new spec. When creating from a requirements document, create one spec per logical phase (multiple calls); title format '01: Name', '02: Name'; markdown: Purpose, Interfaces, Tasks table (1.0/1.1), Test criteria. Do not create tasks in the same step — task creation is always a separate step after all specs exist.", serde_json::json!({"type":"object","properties":{"title":{"type":"string"},"markdown_contents":{"type":"string"}},"required":["title","markdown_contents"]})),
        compact_tool("update_spec", "Update an existing spec's title, order, or full contents. Use the UUID spec_id from list_specs. Pass if_match (the content_hash from get_spec) to refuse the write if the spec changed since you read it. Prefer update_spec_section or append_to_spec for small edits to large specs.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"},"title":{"type":"string"},"order_index":{"type":"integer","description":"New sort position for the spec."},"markdown_contents":{"type":"string"},"if_match":{"type":"string","description":"Optimistic-concurrency token (content_hash from get_spec). The write is refused with a conflict error if the spec's current body no longer matches."}},"required":["spec_id"]})),
        compact_tool("update_spec_section", "Replace the body of a single `## ` section of a spec without re-sending the whole document. Use for small edits to large specs. section_heading matches case-insensitively, with or without the leading `## `. Pass if_match (content_hash from get_spec) to refuse stale writes.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"},"section_heading":{"type":"string","description":"Heading of the `## ` section to replace, e.g. `## Goals` or `Goals`."},"new_body":{"type":"string","description":"Replacement markdown for that section's body (the heading line is kept)."},"if_match":{"type":"string","description":"Optimistic-concurrency token (content_hash from get_spec)."}},"required":["spec_id","section_heading","new_body"]})),
        compact_tool("append_to_spec", "Append a markdown block to the end of a spec without re-sending the existing body. Pass if_match (content_hash from get_spec) to refuse stale writes.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"},"markdown":{"type":"string","description":"Markdown to append; separated from the existing body by a blank line."},"if_match":{"type":"string","description":"Optimistic-concurrency token (content_hash from get_spec)."}},"required":["spec_id","markdown"]})),
        compact_tool("delete_spec", "Delete a spec and its tasks from the project. Use the UUID spec_id from list_specs.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"}},"required":["spec_id"]})),
    ]
}

fn task_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool("list_tasks", "List all tasks in the project, optionally filtered by UUID spec_id from list_specs.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"}},"required":[]})),
        compact_tool("create_task", "Create a new task under a spec. Use the UUID spec_id from list_specs. Only use after specs exist; never create tasks in the same turn as creating specs — spec creation and task creation are two distinct steps.", serde_json::json!({"type":"object","properties":{"spec_id":{"type":"string"},"title":{"type":"string"},"description":{"type":"string"},"dependency_ids":{"type":"array","items":{"type":"string"},"description":"UUIDs of tasks this task depends on (from list_tasks)"}},"required":["spec_id","title","description"]})),
        compact_tool("update_task", "Update individual fields of a task: title, description, status, order_index, or dependency_ids. Only the fields you pass are changed. Use the UUID task_id from list_tasks.", serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"title":{"type":"string"},"description":{"type":"string"},"status":{"type":"string","enum":["pending","ready","in_progress","blocked","done","failed"]},"order_index":{"type":"integer","description":"New sort position within the spec."},"dependency_ids":{"type":"array","items":{"type":"string"},"description":"Replacement list of task UUIDs this task depends on."}},"required":["task_id"]})),
        compact_tool("delete_task", "Delete a task from the project. Requires UUID task_id and parent UUID spec_id from list_tasks.", serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"spec_id":{"type":"string"}},"required":["task_id","spec_id"]})),
        compact_tool("transition_task", "Transition a task to a new status (e.g. pending -> ready, ready -> done).", serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"status":{"type":"string","enum":["pending","ready","in_progress","blocked","done","failed"]}},"required":["task_id","status"]})),
        compact_tool("run_task", "Trigger execution of a single task by the dev-loop engine.", serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"]})),
    ]
}

fn project_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool("get_project", "Get the current project's details (name, folder, status, etc.).", serde_json::json!({"type":"object","properties":{},"required":[]})),
        compact_tool("update_project", "Update the current project's name, description, build_command, or test_command. Commands must be valid shell commands with no extra text.", serde_json::json!({"type":"object","properties":{"name":{"type":"string"},"description":{"type":"string"},"build_command":{"type":"string"},"test_command":{"type":"string"}},"required":[]})),
        compact_tool(
            "assign_agent_to_project",
            "Hire an existing template agent into the current project. Requires the template agent_id (from list_agents). Creates a new AgentInstance bound to the project and returns its agent_instance_id, which can then be addressed via send_to_agent / delegate_task. Idempotent error path: if the same template is already assigned, returns error_code=\"already_assigned\" with the existing agent_instance_id in the payload so the caller can re-use the existing instance instead of retrying.",
            serde_json::json!({
                "type":"object",
                "properties":{
                    "agent_id":{"type":"string","description":"Template agent_id to hire (UUID from list_agents)."}
                },
                "required":["agent_id"]
            }),
        ),
    ]
}

fn dev_loop_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        compact_tool("start_dev_loop", "Start the autonomous dev loop for the project. It will pick up ready tasks and execute them.", serde_json::json!({"type":"object","properties":{},"required":[]})),
        compact_tool("pause_dev_loop", "Pause the currently running dev loop.", serde_json::json!({"type":"object","properties":{},"required":[]})),
        compact_tool("stop_dev_loop", "Stop the currently running dev loop.", serde_json::json!({"type":"object","properties":{},"required":[]})),
    ]
}

// ============================================================================
// Engine tools
// ============================================================================

pub fn engine_specific_tools() -> Vec<ToolDefinition> {
    // NOTE (Layer 0): the conventional write_file/edit_file/delete_file
    // write primitives reach the dev-loop agent via the
    // `ToolProfile::Engine` inheritance from `ToolProfile::Core`
    // (see `crates/aura-tools/src/catalog.rs`), so they must NOT be
    // re-added here — doing so trips the no-duplicate-per-profile
    // invariant in `catalog::tests::no_duplicate_names_in_any_profile`.
    vec![
        tool(
            "task_done",
            "Signal that the current task is complete. Call this when you have finished all changes and verified they compile. Provide notes summarizing what you did, optionally follow-up task suggestions, and a reasoning array with key decisions.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "notes": { "type": "string", "description": "Summary of what was done" },
                    "no_changes_needed": {
                        "type": "boolean",
                        "description": "Set true only when the task is already satisfied and no write_file/edit_file/delete_file changes are required; explain why in notes."
                    },
                    "follow_ups": {
                        "type": "array",
                        "description": "Optional follow-up task suggestions",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["title", "description"]
                        }
                    },
                    "reasoning": {
                        "type": "array",
                        "description": "Key decisions and their rationale (optional but encouraged)",
                        "items": { "type": "string" }
                    }
                },
                "required": ["notes"]
            }),
        ),
        tool(
            "get_task_context",
            "Retrieve the full context for the current task including the spec, task description, and any prior execution notes.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        ),
        tool(
            "submit_plan",
            "Optional: record your implementation plan for the transcript. \
             The plan is surfaced to the operator. Calling submit_plan resets \
             the agent loop's exploration tracking, so it's most useful after \
             you finish exploring and before you start editing. It is never \
             required — you can edit files at any time.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "approach": {
                        "type": "string",
                        "description": "Your implementation strategy (2-4 sentences)"
                    },
                    "files_to_modify": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Existing files you will edit"
                    },
                    "files_to_create": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "New files you will create"
                    },
                    "key_decisions": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Key design decisions and why"
                    }
                },
                "required": ["approach", "files_to_modify", "files_to_create"]
            }),
        ),
    ]
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_property_descriptions_removes_descriptions() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The name" },
                "age": { "type": "integer", "description": "The age" }
            }
        });
        let stripped = strip_property_descriptions(schema);
        let props = stripped.get("properties").unwrap();
        assert!(props.get("name").unwrap().get("description").is_none());
        assert!(props.get("age").unwrap().get("description").is_none());
    }

    #[test]
    fn streaming_tools_opt_into_eager_input_streaming() {
        let fs_tools = file_io_tools();
        let spec_tools = spec_tool_definitions();

        for name in ["write_file", "edit_file"] {
            let t = fs_tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing {name} in file_io_tools"));
            assert_eq!(
                t.eager_input_streaming,
                Some(true),
                "{name} must opt into fine-grained tool streaming"
            );
        }

        for name in ["create_spec", "update_spec"] {
            let t = spec_tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("missing {name} in spec_tool_definitions"));
            assert_eq!(
                t.eager_input_streaming,
                Some(true),
                "{name} must opt into fine-grained tool streaming"
            );
        }

        let read = fs_tools.iter().find(|t| t.name == "read_file").unwrap();
        assert_eq!(
            read.eager_input_streaming, None,
            "read_file has nothing large to stream; should not set the flag"
        );
    }

    #[test]
    fn task_done_schema_exposes_no_changes_needed_escape_hatch() {
        let tools = engine_specific_tools();
        let task_done = tools
            .iter()
            .find(|tool| tool.name == "task_done")
            .expect("task_done schema exists");
        let props = task_done
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .expect("task_done has properties");

        assert_eq!(
            props
                .get("no_changes_needed")
                .and_then(|schema| schema.get("type"))
                .and_then(serde_json::Value::as_str),
            Some("boolean")
        );
        assert!(props
            .get("no_changes_needed")
            .and_then(|schema| schema.get("description"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .contains("no write_file/edit_file/delete_file changes are required"));
    }

    #[test]
    fn strip_property_descriptions_preserves_types() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" },
                "count": { "type": "integer", "description": "Count" }
            }
        });
        let stripped = strip_property_descriptions(schema);
        let props = stripped.get("properties").unwrap();
        assert_eq!(props.get("path").unwrap().get("type").unwrap(), "string");
        assert_eq!(props.get("count").unwrap().get("type").unwrap(), "integer");
    }
}
