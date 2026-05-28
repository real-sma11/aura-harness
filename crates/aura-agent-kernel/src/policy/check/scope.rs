//! `target_*` arg scope validation against [`aura_core::AgentScope`].

/// Inspect `args` for conventional `target_*` keys and verify they fall
/// within `scope`. Absence of a target key means the tool is not
/// targeting that axis and the check is skipped.
pub(super) fn scope_violation(
    scope: &aura_core::AgentScope,
    args: &serde_json::Value,
) -> Option<String> {
    if scope.is_universe() {
        return None;
    }
    let obj = args.as_object()?;
    if let Some(id) = obj.get("target_org_id").and_then(|v| v.as_str()) {
        if !scope.orgs.is_empty() && !scope.orgs.iter().any(|o| o == id) {
            return Some(format!("permissions: target out of scope (org '{id}')"));
        }
    }
    if let Some(id) = obj.get("target_project_id").and_then(|v| v.as_str()) {
        if !scope.projects.is_empty() && !scope.projects.iter().any(|p| p == id) {
            return Some(format!("permissions: target out of scope (project '{id}')"));
        }
    }
    if let Some(id) = obj.get("target_agent_id").and_then(|v| v.as_str()) {
        if !scope.agent_ids.is_empty() && !scope.agent_ids.iter().any(|a| a == id) {
            return Some(format!("permissions: target out of scope (agent '{id}')"));
        }
    }
    None
}
