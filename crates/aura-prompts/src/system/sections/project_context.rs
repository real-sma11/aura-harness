//! `<project_context>`-bound section.
//!
//! Consolidates the project metadata + host platform notice into a
//! single `<project_context>...</project_context>` envelope shared by
//! the dev-loop and chat paths. Blank fields are omitted so a project
//! row with no description / no build / test commands doesn't emit
//! `description: \n` etc.

use crate::descriptors::ProjectInfo;

/// Render the project-context block.
///
/// `name` / `folder` / `platform` are always emitted. `description` /
/// `build_command` / `test_command` are omitted when blank / unset so
/// the rendered body stays compact for under-configured projects.
#[must_use]
pub fn render(project: &ProjectInfo<'_>) -> String {
    let mut body = String::new();
    if let Some(pid) = project.project_id.map(str::trim).filter(|s| !s.is_empty()) {
        body.push_str(&format!("project_id: {pid}\n"));
    }
    body.push_str(&format!("project_name: {}\n", project.name));
    let description = project.description.trim();
    if !description.is_empty() {
        body.push_str(&format!("description: {description}\n"));
    }
    body.push_str(&format!("folder: {}\n", project.folder_path));
    if let Some(build) = project
        .build_command
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body.push_str(&format!("build_command: {build}\n"));
    }
    if let Some(test) = project
        .test_command
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        body.push_str(&format!("test_command: {test}\n"));
    }
    body.push_str(&format!(
        "platform: {}\n",
        super::dev_loop_workflow::platform_info_string()
    ));
    format!("<project_context>\n{body}</project_context>")
}
