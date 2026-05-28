//! Skill activation — argument substitution and content rendering.
//!
//! When a skill is invoked with arguments, placeholders in the skill body are
//! replaced with concrete values before the content is returned.

use crate::error::SkillError;
use crate::types::{Skill, SkillActivation};

/// Split an argument string respecting single and double quotes.
///
/// Unquoted regions are split on whitespace. Inside `"..."` or `'...'`,
/// whitespace is preserved. Backslash inside double quotes escapes the
/// next character. Returns the list of individual arguments.
fn split_quoted_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' if in_double => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(c);
            }
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// Activate a skill by substituting placeholders with the provided arguments.
///
/// Supported substitutions:
/// - `$ARGUMENTS` — replaced with the full argument string
/// - `$ARGUMENTS[N]` — replaced with the Nth (0-based) argument (supports quoted args with spaces)
/// - `$N` (e.g. `$0`, `$1`) — shorthand for `$ARGUMENTS[N]`
/// - `${SKILL_DIR}` — replaced with the skill's directory path
///
/// Backtick command injection is recognised but **not yet implemented**;
/// those placeholders are left as-is.
///
/// # Errors
///
/// Returns [`SkillError::Activation`] if argument substitution fails.
pub fn activate(skill: &Skill, arguments: &str) -> Result<SkillActivation, SkillError> {
    let args: Vec<String> = split_quoted_args(arguments);
    let dir_str = skill.dir_path.to_string_lossy();

    let mut content = skill.body.clone();

    // ${SKILL_DIR} substitution
    content = content.replace("${SKILL_DIR}", &dir_str);

    // $ARGUMENTS[N] substitution (must come before $ARGUMENTS to avoid partial match)
    let mut i = 0;
    while let Some(start) = content[i..].find("$ARGUMENTS[") {
        let abs_start = i + start;
        if let Some(end) = content[abs_start..].find(']') {
            let idx_str = &content[abs_start + 11..abs_start + end];
            if let Ok(idx) = idx_str.parse::<usize>() {
                let replacement = args.get(idx).map_or("", |s| s.as_str());
                let full_placeholder = &content[abs_start..=abs_start + end].to_string();
                content = content.replacen(full_placeholder, replacement, 1);
            } else {
                i = abs_start + end + 1;
            }
        } else {
            break;
        }
    }

    // $ARGUMENTS substitution (full argument string)
    content = content.replace("$ARGUMENTS", arguments);

    // $N shorthand — replace in reverse order so that `$10` is handled before
    // `$1` (which would otherwise partially match, leaving a stray `0`).
    for (idx, arg) in args.iter().enumerate().rev() {
        let placeholder = format!("${idx}");
        content = content.replace(&placeholder, arg);
    }

    // TODO: `!`command`` injection — execute shell commands embedded in the
    // skill body and replace them with their stdout. Skipped for Phase 1.

    let fork_context = skill
        .frontmatter
        .context
        .as_deref()
        .is_some_and(|c| c == "fork");

    Ok(SkillActivation {
        skill_name: skill.frontmatter.name.clone(),
        rendered_content: content,
        allowed_tools: skill.frontmatter.allowed_tools.clone().unwrap_or_default(),
        fork_context,
        agent_type: skill.frontmatter.agent.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{SkillFrontmatter, SkillSource};
    use std::path::PathBuf;

    fn test_skill(body: &str) -> Skill {
        Skill {
            frontmatter: SkillFrontmatter {
                name: "test-skill".to_string(),
                description: "test".to_string(),
                ..SkillFrontmatter::default()
            },
            body: body.to_string(),
            source: SkillSource::Workspace,
            dir_path: PathBuf::from("/skills/test-skill"),
        }
    }

    #[test]
    fn substitutes_full_arguments() {
        let skill = test_skill("Run: $ARGUMENTS");
        let act = activate(&skill, "deploy production").unwrap();
        assert_eq!(act.rendered_content, "Run: deploy production");
    }

    #[test]
    fn substitutes_indexed_arguments() {
        let skill = test_skill("Env: $ARGUMENTS[0], Target: $ARGUMENTS[1]");
        let act = activate(&skill, "staging us-east-1").unwrap();
        assert_eq!(act.rendered_content, "Env: staging, Target: us-east-1");
    }

    #[test]
    fn substitutes_dollar_n_shorthand() {
        let skill = test_skill("Deploy $0 to $1");
        let act = activate(&skill, "app prod").unwrap();
        assert_eq!(act.rendered_content, "Deploy app to prod");
    }

    #[test]
    fn substitutes_skill_dir() {
        let skill = test_skill("Read ${SKILL_DIR}/config.yaml");
        let act = activate(&skill, "").unwrap();
        assert!(act
            .rendered_content
            .contains("/skills/test-skill/config.yaml"));
    }

    #[test]
    fn dollar_10_not_mangled_by_dollar_1() {
        let skill = test_skill("First=$1 Tenth=$10");
        let act = activate(&skill, "z0 alpha z2 z3 z4 z5 z6 z7 z8 z9 TENTH").unwrap();
        assert_eq!(
            act.rendered_content, "First=alpha Tenth=TENTH",
            "got: {}",
            act.rendered_content
        );
    }

    #[test]
    fn out_of_range_argument_index_becomes_empty() {
        let skill = test_skill("Value: $ARGUMENTS[99]");
        let act = activate(&skill, "only_one").unwrap();
        assert_eq!(act.rendered_content, "Value: ");
    }

    #[test]
    fn empty_arguments_with_dollar_zero() {
        let skill = test_skill("Arg: $0");
        let act = activate(&skill, "").unwrap();
        assert_eq!(act.rendered_content, "Arg: $0");
    }

    #[test]
    fn fork_context_detected() {
        let skill = Skill {
            frontmatter: SkillFrontmatter {
                name: "test-skill".to_string(),
                description: "test".to_string(),
                context: Some("fork".to_string()),
                ..SkillFrontmatter::default()
            },
            body: "body".to_string(),
            source: SkillSource::Workspace,
            dir_path: PathBuf::from("/skills/test-skill"),
        };
        let act = activate(&skill, "").unwrap();
        assert!(act.fork_context);
    }
}
