# Super-agent tool inventory

Phase 2 of the super-agent / harness unification plan ports the CEO
super-agent's full tool surface onto the generic harness tool model.
Each aura-os super-agent tool ends up in exactly one of three
categories:

- **reuse** — a semantically equivalent harness built-in already
  exists; wire the super-agent tool name to the harness tool.
- **extend** — the harness has a close primitive but it is missing a
  small capability; add the capability to the harness.
- **http** — the tool is domain logic owned by `aura-os-server`; expose
  a thin HTTP endpoint from aura-os-server and register a generic
  [`HttpToolDefinition`](../src/http_tool.rs) in the harness.

The **http** column is covered mechanically by `HttpToolDefinition` +
the Claude-shape schema converter in
[`schema.rs`](../src/schema.rs); each domain endpoint becomes one
`HttpToolDefinition` instance at session boot with no per-tool Rust
glue needed. The **reuse** column requires an alias map from
super-agent name → harness name. The **extend** column is the only
one that requires new logic in the harness.

Source of truth for the tool list:
[`crates/aura-os-super-agent/src/tools/mod.rs`](../../../../aura-os/crates/aura-os-super-agent/src/tools/mod.rs)
(registered by aura-os tool list builders such as `with_all_tools` and
`register_process_tools`).

## Tier 1 (always exposed)

| Tool                      | Strategy | aura-os endpoint (http)               | Notes |
|---------------------------|----------|---------------------------------------|-------|
| `create_project`          | http     | POST `/api/projects`                  | |
| `import_project`          | http     | POST `/api/projects/import`           | |
| `list_projects`           | http     | GET `/api/projects`                   | Org-scoped via JWT. |
| `get_project`             | http     | GET `/api/projects/{project_id}`      | |
| `update_project`          | http     | PUT `/api/projects/{project_id}`      | |
| `delete_project`          | http     | DELETE `/api/projects/{project_id}`   | |
| `archive_project`         | http     | POST `/api/projects/{project_id}/archive` | |
| `get_project_stats`       | http     | GET `/api/projects/{project_id}/stats` | |
| `list_agents`             | http     | GET `/api/agents`                     | |
| `get_agent`               | http     | GET `/api/agents/{agent_id}`          | |
| `assign_agent_to_project` | **implemented** (domain tool) | POST `/api/projects/{project_id}/agents` | Hires an existing template `agent_id` into the current project. Pre-flights against `list_project_agents` for duplicate detection; returns `error_code="already_assigned"` with the existing `agent_instance_id` when the template is already present. Gated on `Capability::SpawnAgent` (kernel policy + catalog visibility). Endpoint is the same one the marketplace **Hire** modal calls. |
| `start_dev_loop`          | http     | POST `/api/projects/{project_id}/loop/start` | |
| `pause_dev_loop`          | http     | POST `/api/projects/{project_id}/loop/pause` | |
| `stop_dev_loop`           | http     | POST `/api/projects/{project_id}/loop/stop` | |
| `get_loop_status`         | http     | GET `/api/projects/{project_id}/loop` | |
| `send_to_agent`           | cross-agent (phase 5) | n/a                  | Becomes the universal `send_to_agent` tool gated by `ControlAgent` capability. |
| `get_fleet_status`        | http     | GET `/api/monitor/fleet`              | |
| `get_progress_report`     | http     | GET `/api/monitor/progress`           | |
| `get_project_cost`        | http     | GET `/api/projects/{project_id}/cost` | |
| `get_credit_balance`      | http     | GET `/api/billing/balance`            | |
| `load_domain_tools`       | superseded by `IntentClassifier` | n/a     | Classifier-driven filter replaces the meta-tool; phase 6 can delete it. |

## Tier 2 (classifier-gated)

### spec
| Tool                      | Strategy | aura-os endpoint                      | Notes |
|---------------------------|----------|---------------------------------------|-------|
| `list_specs`              | http     | GET `/api/projects/{project_id}/specs` | |
| `get_spec`                | http     | GET `/api/specs/{spec_id}`            | |
| `create_spec`             | http     | POST `/api/projects/{project_id}/specs` | Eager input streaming. |
| `update_spec`             | http     | PUT `/api/specs/{spec_id}`            | Eager input streaming. |
| `delete_spec`             | http     | DELETE `/api/specs/{spec_id}`         | |
| `generate_specs`          | http     | POST `/api/projects/{project_id}/specs/generate` | |
| `generate_specs_summary`  | http     | POST `/api/projects/{project_id}/specs/summary`  | |

### task
| Tool                | Strategy | aura-os endpoint                      | Notes |
|---------------------|----------|---------------------------------------|-------|
| `list_tasks`        | http     | GET `/api/projects/{project_id}/tasks` | |
| `list_tasks_by_spec`| http     | GET `/api/specs/{spec_id}/tasks`      | |
| `get_task`          | http     | GET `/api/tasks/{task_id}`            | |
| `create_task`       | http     | POST `/api/specs/{spec_id}/tasks`     | |
| `update_task`       | http     | PUT `/api/tasks/{task_id}`            | |
| `delete_task`       | http     | DELETE `/api/tasks/{task_id}`         | |
| `extract_tasks`     | http     | POST `/api/specs/{spec_id}/tasks/extract` | |
| `transition_task`   | http     | POST `/api/tasks/{task_id}/transition`| |
| `retry_task`        | http     | POST `/api/tasks/{task_id}/retry`     | |
| `run_task`          | http     | POST `/api/tasks/{task_id}/run`       | |
| `get_task_output`   | http     | GET `/api/tasks/{task_id}/output`     | |

### agent (tier 2)
| Tool                      | Strategy              | aura-os endpoint                           | Notes |
|---------------------------|-----------------------|--------------------------------------------|-------|
| `create_agent`            | http                  | POST `/api/agents`                         | |
| `update_agent`            | http                  | PUT `/api/agents/{agent_id}`               | |
| `delete_agent`            | http                  | DELETE `/api/agents/{agent_id}`            | |
| `list_agent_instances`    | http                  | GET `/api/agents/{agent_id}/instances`     | |
| `update_agent_instance`   | http                  | PUT `/api/agent-instances/{instance_id}`   | |
| `delete_agent_instance`   | http                  | DELETE `/api/agent-instances/{instance_id}`| |
| `remote_agent_action`     | cross-agent (phase 5) | n/a                                        | Becomes `agent_lifecycle` gated by `ControlAgent`. |

### org / billing / social / monitoring / system / generation / process
| Tool                      | Strategy | aura-os endpoint                                      | Notes |
|---------------------------|----------|-------------------------------------------------------|-------|
| `list_orgs`               | http     | GET `/api/orgs`                                       | |
| `create_org`              | http     | POST `/api/orgs`                                      | |
| `get_org`                 | http     | GET `/api/orgs/{org_id}`                              | |
| `update_org`              | http     | PUT `/api/orgs/{org_id}`                              | |
| `list_members`            | http     | GET `/api/orgs/{org_id}/members`                      | |
| `update_member_role`      | http     | PUT `/api/orgs/{org_id}/members/{user_id}`            | |
| `remove_member`           | http     | DELETE `/api/orgs/{org_id}/members/{user_id}`         | |
| `manage_invites`          | http     | POST `/api/orgs/{org_id}/invites`                     | |
| `get_transactions`        | http     | GET `/api/billing/transactions`                       | |
| `get_billing_account`     | http     | GET `/api/billing/account`                            | |
| `purchase_credits`        | http     | POST `/api/billing/purchase`                          | |
| `list_feed`               | http     | GET `/api/social/feed`                                | |
| `create_post`             | http     | POST `/api/social/posts`                              | |
| `get_post`                | http     | GET `/api/social/posts/{post_id}`                     | |
| `add_comment`             | http     | POST `/api/social/posts/{post_id}/comments`           | |
| `delete_comment`          | http     | DELETE `/api/social/comments/{comment_id}`            | |
| `follow_profile`          | http     | POST `/api/social/follows`                            | |
| `unfollow_profile`        | http     | DELETE `/api/social/follows/{profile_id}`             | |
| `list_follows`            | http     | GET `/api/social/follows`                             | |
| `get_leaderboard`         | http     | GET `/api/monitor/leaderboard`                        | |
| `get_usage_stats`         | http     | GET `/api/monitor/usage`                              | |
| `list_sessions`           | http     | GET `/api/monitor/sessions`                           | |
| `list_log_entries`        | http     | GET `/api/monitor/logs`                               | |
| `browse_files`            | reuse    | harness `list_files`                                  | Alias super-agent name. |
| `read_file`               | reuse    | harness `read_file`                                   | Alias. |
| `get_environment_info`    | http     | GET `/api/system/environment`                         | |
| `get_remote_agent_state`  | cross-agent (phase 5) | n/a                                      | Becomes `get_agent_state` gated by `ReadAgent`. |
| `generate_image`          | http     | POST `/api/generation/image`                          | |
| `generate_3d_model`       | http     | POST `/api/generation/3d`                             | |
| `get_3d_status`           | http     | GET `/api/generation/3d/{job_id}`                     | |
| `create_process`          | http     | POST `/api/processes`                                 | |
| `list_processes`          | http     | GET `/api/processes`                                  | |
| `delete_process`          | http     | DELETE `/api/processes/{process_id}`                  | |
| `trigger_process`         | http     | POST `/api/processes/{process_id}/trigger`            | |
| `list_process_runs`       | http     | GET `/api/processes/{process_id}/runs`                | |

## Summary

- **60 of 63** super-agent tools map directly to existing or
  to-be-added aura-os HTTP endpoints and are covered mechanically by
  `HttpToolDefinition` — no per-tool Rust glue in the harness.
- **2 tools** (`browse_files`, `read_file`) are pure filesystem
  operations and reuse harness built-ins; the only work is a
  super-agent-name → harness-name alias registered in the catalog
  extension used by the super-agent profile.
- **3 tools** (`send_to_agent`, `remote_agent_action`,
  `get_remote_agent_state`) are cross-agent control primitives and
  move to phase 5, implemented once in `aura-tools` as universal
  capability-gated tools (`send_to_agent`, `agent_lifecycle`,
  `get_agent_state`). Phase 2 intentionally does not try to replicate
  them.
- **1 tool** (`load_domain_tools`) is superseded by the
  [`IntentClassifier`](../src/intent_classifier.rs): the classifier
  derives the visible domain set each turn directly from the user
  message, so the meta-tool is no longer required. Phase 6 can delete
  the meta-tool entry from the profile.

Phase 3 wires this inventory up: on session boot for a super-agent
record with `host_mode = "harness"`, the server ships the CEO
[`SuperAgentProfile`](../../../../aura-os/crates/aura-os-super-agent-profile/src/profile.rs)
JSON to the harness, the harness instantiates one
`HttpToolDefinition` per `http`-strategy entry (plus the alias map
for `reuse`), and the `IntentClassifier` gates visibility per turn.
