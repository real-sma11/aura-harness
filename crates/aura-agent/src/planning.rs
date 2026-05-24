//! Task planning types for the optional explore/implement plan handshake.
//!
//! As of harness-v2, tasks start in [`TaskPhase::Implementing`] with an empty
//! plan so write operations (`write_file`, `edit_file`, `delete_file`) and
//! `task_done` are accepted from the first iteration. Calling `submit_plan`
//! is now optional planning metadata — it still records the plan, resets the
//! rolling-outcome window, and bumps the exploration budget, but is no longer
//! required for a task to reach the implement phase.
//
// `Exploring` is preserved in the enum so historical traces, telemetry, and
// the `validate_execution` `reached_implementing` check keep round-tripping;
// nothing in the live code path constructs it any more.
#![allow(dead_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskPhase {
    Exploring,
    Implementing { plan: TaskPlan },
}

impl Default for TaskPhase {
    fn default() -> Self {
        Self::Implementing {
            plan: TaskPlan::empty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPlan {
    pub approach: String,
    pub files_to_modify: Vec<String>,
    pub files_to_create: Vec<String>,
    pub key_decisions: Vec<String>,
}

impl TaskPlan {
    pub const fn empty() -> Self {
        Self {
            approach: String::new(),
            files_to_modify: Vec::new(),
            files_to_create: Vec::new(),
            key_decisions: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.approach.len() < 20 {
            return Err(
                "Plan approach is too brief. Describe your implementation strategy.".into(),
            );
        }
        if self.files_to_modify.is_empty() && self.files_to_create.is_empty() {
            return Err("Plan must specify at least one file to modify or create.".into());
        }
        Ok(())
    }

    pub fn from_tool_input(input: &serde_json::Value) -> Self {
        let approach = input
            .get("approach")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let files_to_modify = input
            .get("files_to_modify")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let files_to_create = input
            .get("files_to_create")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let key_decisions = input
            .get("key_decisions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            approach,
            files_to_modify,
            files_to_create,
            key_decisions,
        }
    }

    pub fn as_context_string(&self) -> String {
        let mut s = format!("Approach: {}\n", self.approach);
        if !self.files_to_modify.is_empty() {
            s.push_str("Files to modify:\n");
            for f in &self.files_to_modify {
                s.push_str(&format!("  - {f}\n"));
            }
        }
        if !self.files_to_create.is_empty() {
            s.push_str("Files to create:\n");
            for f in &self.files_to_create {
                s.push_str(&format!("  - {f}\n"));
            }
        }
        if !self.key_decisions.is_empty() {
            s.push_str("Key decisions:\n");
            for d in &self.key_decisions {
                s.push_str(&format!("  - {d}\n"));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_short_approach() {
        let plan = TaskPlan {
            approach: "short".into(),
            files_to_modify: vec!["foo.rs".into()],
            files_to_create: vec![],
            key_decisions: vec![],
        };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn validate_rejects_no_files() {
        let plan = TaskPlan {
            approach: "A sufficiently long approach description for the plan".into(),
            files_to_modify: vec![],
            files_to_create: vec![],
            key_decisions: vec![],
        };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_plan() {
        let plan = TaskPlan {
            approach: "Implement the feature by modifying the handler module".into(),
            files_to_modify: vec!["src/handler.rs".into()],
            files_to_create: vec![],
            key_decisions: vec!["Use existing error type".into()],
        };
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn from_tool_input_parses_json() {
        let input = serde_json::json!({
            "approach": "Modify the handler to support the new feature",
            "files_to_modify": ["src/handler.rs"],
            "files_to_create": ["src/new_module.rs"],
            "key_decisions": ["Reuse existing types"]
        });
        let plan = TaskPlan::from_tool_input(&input);
        assert_eq!(
            plan.approach,
            "Modify the handler to support the new feature"
        );
        assert_eq!(plan.files_to_modify, vec!["src/handler.rs"]);
        assert_eq!(plan.files_to_create, vec!["src/new_module.rs"]);
        assert_eq!(plan.key_decisions, vec!["Reuse existing types"]);
    }

    #[test]
    fn empty_plan_has_no_fields() {
        let plan = TaskPlan::empty();
        assert!(plan.approach.is_empty());
        assert!(plan.files_to_modify.is_empty());
        assert!(plan.files_to_create.is_empty());
    }

    #[test]
    fn task_phase_defaults_to_implementing_with_empty_plan() {
        // harness-v2 contract: a freshly-constructed TaskPhase must report
        // the implementing variant so `validate_execution`'s
        // `reached_implementing` check is satisfied by any task that
        // produces file ops, regardless of whether `submit_plan` ran.
        // Tests in the dev-loop validation suite rely on this default
        // matching the production executor's initial phase.
        match TaskPhase::default() {
            TaskPhase::Implementing { plan } => assert_eq!(plan, TaskPlan::empty()),
            TaskPhase::Exploring => panic!("default phase must be Implementing"),
        }
    }

    #[test]
    fn as_context_string_formats_plan() {
        let plan = TaskPlan {
            approach: "Implement feature X".into(),
            files_to_modify: vec!["a.rs".into()],
            files_to_create: vec!["b.rs".into()],
            key_decisions: vec!["Use Arc".into()],
        };
        let ctx = plan.as_context_string();
        assert!(ctx.contains("Implement feature X"));
        assert!(ctx.contains("a.rs"));
        assert!(ctx.contains("b.rs"));
        assert!(ctx.contains("Use Arc"));
    }
}
