//! SKILL.md parser — extracts YAML frontmatter and markdown body.

use crate::error::SkillError;
use crate::types::SkillFrontmatter;

/// Validate that a skill name conforms to the naming rules:
/// lowercase ASCII letters, digits, and hyphens only, 1-64 characters.
///
/// # Errors
///
/// Returns [`SkillError::InvalidName`] when the name is empty, too long, or
/// contains characters outside `[a-z0-9-]`.
pub fn validate_name(name: &str) -> Result<(), SkillError> {
    if name.is_empty() || name.len() > 64 {
        return Err(SkillError::InvalidName(format!(
            "name must be 1-64 characters, got {}",
            name.len()
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(SkillError::InvalidName(format!(
            "name must contain only lowercase letters, digits, and hyphens: {name}"
        )));
    }
    Ok(())
}

/// Parse a SKILL.md file's raw text into frontmatter + body.
///
/// The file must start with a `---` YAML frontmatter block. Everything between
/// the first and second `---` is deserialized as [`SkillFrontmatter`]; the rest
/// is returned as the markdown body.
///
/// # Errors
///
/// Returns [`SkillError::Parse`] when delimiters are missing, [`SkillError::Yaml`]
/// when frontmatter is invalid YAML, or [`SkillError::InvalidName`] when the
/// name fails validation.
pub fn parse_skill_md(content: &str) -> Result<(SkillFrontmatter, String), SkillError> {
    let trimmed = content.trim_start();

    if !trimmed.starts_with("---") {
        return Err(SkillError::Parse(
            "SKILL.md must start with --- frontmatter delimiter".into(),
        ));
    }

    let after_first = &trimmed[3..];
    let closing = after_first
        .find("\n---")
        .ok_or_else(|| SkillError::Parse("missing closing --- frontmatter delimiter".into()))?;

    let yaml_block = &after_first[..closing];
    let body_start = closing + 4; // skip the "\n---"
    let body = if body_start < after_first.len() {
        after_first[body_start..]
            .trim_start_matches('\n')
            .to_string()
    } else {
        String::new()
    };

    let frontmatter: SkillFrontmatter = serde_yaml::from_str(yaml_block)?;
    validate_name(&frontmatter.name)?;

    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill_md() {
        let content = r#"---
name: deploy
description: Deploy the application
---
Step 1: Build
Step 2: Push
"#;
        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "deploy");
        assert_eq!(fm.description, "Deploy the application");
        assert!(body.contains("Step 1: Build"));
    }

    #[test]
    fn missing_frontmatter_delimiter() {
        let content = "no frontmatter here";
        assert!(parse_skill_md(content).is_err());
    }

    #[test]
    fn invalid_name_uppercase() {
        let content = "---\nname: Deploy\ndescription: test\n---\nbody";
        assert!(parse_skill_md(content).is_err());
    }

    #[test]
    fn name_too_long() {
        let long_name: String = "a".repeat(65);
        let content = format!("---\nname: {long_name}\ndescription: test\n---\nbody");
        assert!(parse_skill_md(&content).is_err());
    }

    #[test]
    fn valid_name_formats() {
        assert!(validate_name("deploy").is_ok());
        assert!(validate_name("my-skill-2").is_ok());
        assert!(validate_name("a").is_ok());

        assert!(validate_name("").is_err());
        assert!(validate_name("Deploy").is_err());
        assert!(validate_name("my_skill").is_err());
    }

    #[test]
    fn invalid_yaml_content() {
        let content = "---\n: invalid: yaml: [[\n---\nbody";
        let result = parse_skill_md(content);
        assert!(result.is_err());
    }

    #[test]
    fn missing_closing_delimiter() {
        let content = "---\nname: test\ndescription: no closing\n";
        let result = parse_skill_md(content);
        assert!(result.is_err());
    }

    #[test]
    fn empty_body_after_frontmatter() {
        let content = "---\nname: test\ndescription: empty body\n---";
        let (fm, body) = parse_skill_md(content).unwrap();
        assert_eq!(fm.name, "test");
        assert!(body.is_empty());
    }

    #[test]
    fn name_with_unicode() {
        // Embed the soft-hyphen via its explicit unicode escape so the file
        // stays free of invisible control characters (clippy::invisible_characters).
        let content = "---\nname: dé\u{AD}ploy\ndescription: test\n---\nbody";
        assert!(parse_skill_md(content).is_err());
    }

    #[test]
    fn name_with_underscore() {
        let content = "---\nname: my_skill\ndescription: test\n---\nbody";
        assert!(parse_skill_md(content).is_err());
    }
}
