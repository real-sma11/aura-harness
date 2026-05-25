//! `<project_context>`-bound section.
//!
//! For PR B this emits the chat-style "## Current Project" prose block
//! that `build_chat_system_prompt` historically appended inline, so
//! the chat snapshots stay byte-identical. Dev-loop callers do not
//! invoke this section in PR B (their build/test commands and platform
//! info are still inlined into [`super::dev_loop_workflow`] for
//! byte-identical output).
//!
//! PR C will flip this to the canonical `<project_context>
//! ...</project_context>` schema and start using it from the dev-loop
//! path as well.

use crate::prompts::ProjectInfo;

/// Render the chat-style project-context block.
///
/// Format mirrors the legacy `build_chat_system_prompt` inline
/// `format!("\n\n## Current Project\n- ...")` exactly, including the
/// leading blank line and the `(not set)` placeholders for missing
/// build / test commands.
#[must_use]
pub(crate) fn render(project: &ProjectInfo<'_>) -> String {
    format!(
        "\n\n## Current Project\n\
         - **Name**: {name}\n\
         - **Description**: {description}\n\
         - **Folder**: {folder}\n\
         - **Build**: {build}\n\
         - **Test**: {test}\n",
        name = project.name,
        description = project.description,
        folder = project.folder_path,
        build = project.build_command.unwrap_or("(not set)"),
        test = project.test_command.unwrap_or("(not set)"),
    )
}
