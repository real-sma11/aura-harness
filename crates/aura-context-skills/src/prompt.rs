//! System-prompt injection — formats skill metadata as compact XML.

use std::fmt::Write;
use std::path::Path;

use crate::types::SkillMeta;

/// Entry for full skill injection (includes body content).
pub struct SkillPromptEntry<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub body: &'a str,
    pub dir_path: &'a Path,
}

/// Render a list of skill metadata entries as an `<available_skills>` XML block
/// suitable for injection into a system prompt.
///
/// Returns an empty string when no skills are provided.
#[must_use]
pub fn render_skills_xml(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut buf = String::from("<available_skills>\n");
    for s in skills {
        let _ = writeln!(
            buf,
            "<skill name=\"{}\" description=\"{}\" location=\"{}\"/>",
            xml_escape(&s.name),
            xml_escape(&s.description),
            xml_escape(&s.source.to_string()),
        );
    }
    buf.push_str("</available_skills>");
    buf
}

/// Render full skill entries with their body content for prompt injection.
///
/// Each skill is rendered as an `<agent_skill>` element containing the
/// description and full SKILL.md body, so the agent can follow the
/// instructions directly without needing to read external files.
#[must_use]
pub fn render_full_skills_xml(skills: &[SkillPromptEntry<'_>]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut buf = String::from(
        "<agent_skills>\n\
         When users ask you to perform tasks, check if any of the available skills below are relevant. \
         Each skill contains instructions you should follow when the skill applies.\n\n",
    );
    for s in skills {
        let _ = write!(
            buf,
            "<agent_skill name=\"{}\" skillPath=\"{}\">",
            xml_escape(s.name),
            xml_escape(&s.dir_path.display().to_string()),
        );
        let _ = write!(buf, "{}", xml_escape(s.description));
        if !s.body.is_empty() {
            let _ = write!(buf, "\n\n{}", xml_escape(s.body));
        }
        buf.push_str("</agent_skill>\n");
    }
    buf.push_str("</agent_skills>");
    buf
}

/// Inject skill metadata into a system prompt string.
///
/// Appends the rendered XML block after a blank line separator.
pub fn inject_into_prompt(system_prompt: &mut String, skills: &[SkillMeta]) {
    let xml = render_skills_xml(skills);
    if xml.is_empty() {
        return;
    }
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&xml);
}

/// Inject full skill content into a system prompt string.
///
/// Appends the rendered XML block (with skill bodies) after a blank line separator.
pub fn inject_full_skills(system_prompt: &mut String, skills: &[SkillPromptEntry<'_>]) {
    let xml = render_full_skills_xml(skills);
    if xml.is_empty() {
        return;
    }
    system_prompt.push_str("\n\n");
    system_prompt.push_str(&xml);
}

/// Minimal XML escaping for attribute values.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SkillSource;

    #[test]
    fn empty_skills_produce_empty_string() {
        assert_eq!(render_skills_xml(&[]), "");
    }

    #[test]
    fn single_skill_xml() {
        let meta = vec![SkillMeta {
            name: "deploy".to_string(),
            description: "Deploy the app".to_string(),
            source: SkillSource::Workspace,
            model_invocable: true,
            user_invocable: true,
            requested_paths: vec![],
            requested_commands: vec![],
        }];
        let xml = render_skills_xml(&meta);
        assert!(xml.contains("<available_skills>"));
        assert!(xml.contains("name=\"deploy\""));
        assert!(xml.contains("location=\"workspace\""));
    }

    #[test]
    fn inject_appends_to_prompt() {
        let mut prompt = "You are an assistant.".to_string();
        let meta = vec![SkillMeta {
            name: "test".to_string(),
            description: "A test skill".to_string(),
            source: SkillSource::Personal,
            model_invocable: true,
            user_invocable: false,
            requested_paths: vec![],
            requested_commands: vec![],
        }];
        inject_into_prompt(&mut prompt, &meta);
        assert!(prompt.starts_with("You are an assistant."));
        assert!(prompt.contains("<available_skills>"));
    }

    #[test]
    fn xml_escaping() {
        let meta = vec![SkillMeta {
            name: "test".to_string(),
            description: "Use <special> & \"chars\"".to_string(),
            source: SkillSource::Bundled,
            model_invocable: true,
            user_invocable: false,
            requested_paths: vec![],
            requested_commands: vec![],
        }];
        let xml = render_skills_xml(&meta);
        assert!(xml.contains("&lt;special&gt;"));
        assert!(xml.contains("&amp;"));
        assert!(xml.contains("&quot;chars&quot;"));
    }

    #[test]
    fn extra_source_with_special_chars_escaped() {
        let meta = vec![SkillMeta {
            name: "extra-skill".to_string(),
            description: "test".to_string(),
            source: SkillSource::Extra(std::path::PathBuf::from("/path/<with>&\"special\"")),
            model_invocable: true,
            user_invocable: false,
            requested_paths: vec![],
            requested_commands: vec![],
        }];
        let xml = render_skills_xml(&meta);
        assert!(xml.contains("&lt;with&gt;"));
        assert!(xml.contains("&amp;"));
        assert!(xml.contains("&quot;special&quot;"));
        assert!(!xml.contains("location=\"extra:/path/<with>"));
    }

    #[test]
    fn multiple_skills_all_present() {
        let meta = vec![
            SkillMeta {
                name: "alpha".to_string(),
                description: "first".to_string(),
                source: SkillSource::Workspace,
                model_invocable: true,
                user_invocable: true,
                requested_paths: vec![],
                requested_commands: vec![],
            },
            SkillMeta {
                name: "beta".to_string(),
                description: "second".to_string(),
                source: SkillSource::Personal,
                model_invocable: false,
                user_invocable: true,
                requested_paths: vec![],
                requested_commands: vec![],
            },
        ];
        let xml = render_skills_xml(&meta);
        assert!(xml.contains("name=\"alpha\""));
        assert!(xml.contains("name=\"beta\""));
    }
}
