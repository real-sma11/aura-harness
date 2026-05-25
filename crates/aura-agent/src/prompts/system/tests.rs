use super::*;
use crate::prompts::AgentIdentity;

fn test_project(folder: &str) -> ProjectInfo<'_> {
    ProjectInfo {
        name: "TestProj",
        description: "Test project description",
        folder_path: folder,
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    }
}

#[test]
fn agentic_prompt_includes_build_command() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);
    assert!(prompt.contains("cargo build"));
    assert!(prompt.contains("cargo test"));
}

#[test]
fn agentic_prompt_includes_definition_of_done_hard_gate() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);
    assert!(
        prompt.contains("hard gate"),
        "task_done hard-gate language missing: {prompt}"
    );
    assert!(
        prompt.contains("task_done only when"),
        "task_done deferral instruction missing: {prompt}"
    );
    assert!(
        prompt.contains("cargo test"),
        "test command reference missing from hard-gate line: {prompt}"
    );
}

#[test]
fn agentic_prompt_no_longer_tells_agent_to_ignore_pre_existing_failures() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);
    assert!(
        !prompt.contains("IGNORE them"),
        "system prompt still tells agent to IGNORE pre-existing failures"
    );
    assert!(
        !prompt.contains("If they are pre-existing and unrelated to your changes"),
        "system prompt still contains the old pre-existing-failure escape hatch"
    );
}

#[test]
fn agentic_prompt_no_longer_advertises_exploration_budget() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        !prompt.contains("EXPLORATION BUDGET"),
        "EXPLORATION BUDGET section was supposed to be removed: {prompt}"
    );
    assert!(
        !prompt.contains("read-only tool calls"),
        "per-call exploration cap prose was supposed to be removed: {prompt}"
    );
    assert!(
        !prompt.contains("Each file can be read at most"),
        "per-file read-cap prose was supposed to be removed: {prompt}"
    );
    assert!(
        !prompt.contains("the only legal moves are apply_patch / task_done"),
        "post-budget legal-move prose was supposed to be removed: {prompt}"
    );
}

#[test]
fn agentic_prompt_leads_with_action_oriented_move_set() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        prompt.contains("Edit code with apply_patch. Finish with task_done."),
        "action-oriented lead line missing: {prompt}"
    );
    assert!(
        !prompt.contains("Workflow:"),
        "old numbered Workflow header should be gone: {prompt}"
    );
    assert!(
        !prompt.contains("1. Explore"),
        "old step-1 Explore prose should be gone: {prompt}"
    );
    assert!(
        !prompt.contains("submit_plan"),
        "prompt no longer advertises submit_plan: {prompt}"
    );
}

#[test]
fn agentic_prompt_promotes_no_changes_needed_in_rules() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        prompt.contains("no_changes_needed: true"),
        "no_changes_needed branch missing from invariants: {prompt}"
    );
    assert!(
        prompt.contains("If no changes are needed"),
        "no_changes_needed conditional phrasing missing: {prompt}"
    );
}

#[test]
fn agentic_prompt_no_longer_includes_tool_call_discipline_section() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        !prompt.contains("Tool-call discipline:"),
        "tool-call discipline section was supposed to be removed: {prompt}"
    );
    assert!(
        !prompt.contains("32000 bytes per call"),
        "write_file chunk-guard prose was supposed to be removed: {prompt}"
    );
    assert!(
        !prompt.contains("<tool_discipline>"),
        "empty <tool_discipline> tag must not be emitted: {prompt}"
    );
}

#[test]
fn agentic_prompt_uses_test_command_env_override_when_set() {
    use crate::task_executor::TEST_COMMAND_OVERRIDE_ENV;
    let prev = std::env::var(TEST_COMMAND_OVERRIDE_ENV).ok();

    std::env::set_var(TEST_COMMAND_OVERRIDE_ENV, "pytest -q -k smoke");

    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None);
    assert!(
        prompt.contains("pytest -q -k smoke"),
        "env override must surface in the prompt"
    );

    match prev {
        Some(v) => std::env::set_var(TEST_COMMAND_OVERRIDE_ENV, v),
        None => std::env::remove_var(TEST_COMMAND_OVERRIDE_ENV),
    }
}

#[test]
fn chat_system_prompt_uses_base_when_custom_empty() {
    let project = test_project("/nonexistent/path");
    let prompt = build_chat_system_prompt(&project, "");
    assert!(prompt.starts_with("<chat_capabilities>\n"));
    assert!(prompt.contains(CHAT_SYSTEM_PROMPT_BASE));
    assert!(prompt.contains("TestProj"));
}

#[test]
fn chat_system_prompt_prepends_custom() {
    let project = test_project("/nonexistent/path");
    let prompt = build_chat_system_prompt(&project, "Custom instructions here.");
    assert!(prompt.starts_with("Custom instructions here."));
    assert!(prompt.contains(CHAT_SYSTEM_PROMPT_BASE));
    assert!(prompt.contains("TestProj"));
}

#[test]
fn chat_system_prompt_includes_project_details() {
    let project = ProjectInfo {
        name: "MyApp",
        description: "A web application",
        folder_path: "/nonexistent/path",
        build_command: Some("npm run build"),
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");
    assert!(prompt.contains("MyApp"));
    assert!(prompt.contains("A web application"));
    assert!(prompt.contains("npm run build"));
    assert!(
        !prompt.contains("test_command:"),
        "blank test_command must be omitted from <project_context>"
    );
}

#[test]
fn chat_system_prompt_drops_workspace_overview_helpers() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
    std::fs::write(dir.path().join("package.json"), "{}").unwrap();

    let project = ProjectInfo {
        name: "MultiStack",
        description: "",
        folder_path: &dir.path().to_string_lossy(),
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");
    assert!(
        !prompt.contains("### Project Structure"),
        "Project Structure overview must not appear in the chat prompt: {prompt}"
    );
    assert!(
        !prompt.contains("### Key Config Files"),
        "Key Config Files overview must not appear in the chat prompt: {prompt}"
    );
    assert!(
        !prompt.contains("**Tech Stack**"),
        "Tech Stack overview must not appear in the chat prompt: {prompt}"
    );
}

#[test]
fn chat_system_prompt_includes_agents_md_when_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "Always run cargo check before tests.\nNo emojis.\n",
    )
    .unwrap();

    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "WithAgents",
        description: "",
        folder_path: &folder,
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");

    assert!(
        prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "chat prompt is missing the <agents_md path=\"...\"> opening tag"
    );
    assert!(
        prompt.contains("</agents_md>"),
        "chat prompt is missing the closing </agents_md> tag"
    );
    assert!(
        prompt.contains("Always run cargo check before tests."),
        "chat prompt did not include AGENTS.md body"
    );
}

#[test]
fn chat_system_prompt_handles_case_insensitive_variants() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("agents.md"), "Lowercase variant body.").unwrap();

    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "LowerAgents",
        description: "",
        folder_path: &folder,
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");

    assert!(prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX));
    assert!(prompt.contains("Lowercase variant body."));
    assert!(
        prompt.contains("path=\"AGENTS.md\"") || prompt.contains("path=\"agents.md\""),
        "expected one of the recognised path attribute values in the rendered prompt"
    );
}

#[test]
fn chat_system_prompt_omits_agents_md_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "NoAgents",
        description: "",
        folder_path: &folder,
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "chat prompt unexpectedly includes <agents_md> when no file is present"
    );
}

#[test]
fn chat_system_prompt_skips_agents_md_when_over_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    let oversize = "x".repeat(AGENTS_MD_MAX_BYTES + 1);
    std::fs::write(dir.path().join("AGENTS.md"), &oversize).unwrap();

    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "BigAgents",
        description: "",
        folder_path: &folder,
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "oversize AGENTS.md must be skipped, not truncated"
    );
    assert!(
        !prompt.contains(&oversize),
        "oversize AGENTS.md content must never reach the system prompt"
    );
}

#[test]
fn chat_system_prompt_skips_agents_md_when_folder_missing() {
    let project = ProjectInfo {
        name: "Ghost",
        description: "",
        folder_path: "/definitely/does/not/exist/aura/agents/md/test",
        build_command: None,
        test_command: None,
    };
    let prompt = build_chat_system_prompt(&project, "");

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "non-existent folder_path must not surface an <agents_md> section"
    );
}

#[test]
fn agentic_prompt_includes_agents_md_when_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "Use raw string literals for multi-line Rust strings.",
    )
    .unwrap();

    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "AgenticAgents",
        description: "",
        folder_path: &folder,
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    };
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX));
    assert!(prompt.contains("Use raw string literals for multi-line Rust strings."));
}

#[test]
fn agentic_prompt_omits_agents_md_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().to_string_lossy().into_owned();
    let project = ProjectInfo {
        name: "AgenticNoAgents",
        description: "",
        folder_path: &folder,
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    };
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "agentic prompt unexpectedly includes <agents_md> when no file is present"
    );
}

#[test]
fn agentic_prompt_skips_agents_md_when_folder_missing() {
    let project = test_project("/definitely/does/not/exist/aura/agentic/test");
    let prompt = agentic_execution_system_prompt(&project, None);

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_TAG_PREFIX),
        "non-existent folder_path must not surface <agents_md> in the agentic prompt"
    );
}

#[test]
fn dev_loop_prompt_with_identity_emits_every_section_in_order() {
    let project = test_project("/nonexistent");
    let skills = vec!["Rust".to_string(), "TypeScript".to_string()];
    let agent = AgentInfo {
        identity: Some(AgentIdentity {
            name: "Atlas",
            role: "Engineer",
            personality: "Precise and methodical.",
        }),
        skills: &skills,
        system_prompt: Some("Use TDD on every change."),
    };
    let prompt = agentic_execution_system_prompt(&project, Some(&agent));

    for tag in [
        "<agent_identity>",
        "<agent_skills>",
        "<agent_system_prompt>",
        "<project_context>",
        "<dev_loop_workflow>",
    ] {
        assert!(
            prompt.contains(tag),
            "{tag} missing from identity-populated dev-loop prompt: {prompt}"
        );
    }
    assert!(
        !prompt.contains("<tool_discipline>"),
        "<tool_discipline> stays unrendered when its renderer returns None"
    );

    let order = [
        "<agent_identity>",
        "<agent_skills>",
        "<agent_system_prompt>",
        "<project_context>",
        "<dev_loop_workflow>",
    ];
    let mut last = 0usize;
    for tag in order {
        let idx = prompt.find(tag).expect("tag present above");
        assert!(
            idx >= last,
            "expected {tag} to appear at or after offset {last}, but found it at {idx}; prompt:\n{prompt}"
        );
        last = idx;
    }
}

const SNAPSHOT_DIR: &str = "__snapshots__";

fn snapshot_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/prompts/system")
        .join(SNAPSHOT_DIR)
        .join(format!("{name}.txt"))
}

fn assert_snapshot(name: &str, actual: &str) {
    let path = snapshot_path(name);
    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create snapshot dir");
        }
        std::fs::write(&path, actual).expect("write snapshot");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "snapshot {} could not be read ({err}); rerun the test with `UPDATE_SNAPSHOTS=1` to generate it",
            path.display()
        )
    });
    let expected_norm = expected.replace("\r\n", "\n");
    assert_eq!(
        expected_norm, actual,
        "snapshot {} mismatch",
        path.display()
    );
}

fn scrub(s: &str, dir: &str) -> String {
    let mut out = s.replace(dir, "<TEMPDIR>");
    let norm = dir.replace('\\', "/");
    if norm != dir {
        out = out.replace(&norm, "<TEMPDIR>");
    }
    let platform = platform_info_string();
    out = out.replace(platform, "<PLATFORM_INFO>");
    out
}

fn demo_project(folder: &str) -> ProjectInfo<'_> {
    ProjectInfo {
        name: "Demo",
        description: "A demo project.",
        folder_path: folder,
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    }
}

fn with_test_command_override_unset<F: FnOnce()>(f: F) {
    let key = crate::task_executor::TEST_COMMAND_OVERRIDE_ENV;
    let prev = std::env::var(key).ok();
    std::env::remove_var(key);
    f();
    match prev {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}

#[test]
fn snapshot_dev_loop_default() {
    with_test_command_override_unset(|| {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        let project = demo_project(&folder);
        let prompt = agentic_execution_system_prompt(&project, None);
        let scrubbed = scrub(&prompt, &folder);
        assert_snapshot("dev_loop_default", &scrubbed);
    });
}

#[test]
fn snapshot_dev_loop_with_agents_md() {
    with_test_command_override_unset(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "Always run cargo check before tests.\nNo emojis.\n",
        )
        .unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        let project = demo_project(&folder);
        let prompt = agentic_execution_system_prompt(&project, None);
        let scrubbed = scrub(&prompt, &folder);
        assert_snapshot("dev_loop_with_agents_md", &scrubbed);
    });
}

#[test]
fn snapshot_dev_loop_with_identity() {
    with_test_command_override_unset(|| {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        let project = demo_project(&folder);
        let skills = vec!["Rust".to_string(), "TypeScript".to_string()];
        let agent = AgentInfo {
            identity: Some(AgentIdentity {
                name: "Atlas",
                role: "Engineer",
                personality: "Precise and methodical.",
            }),
            skills: &skills,
            system_prompt: Some("Use TDD on every change."),
        };
        let prompt = agentic_execution_system_prompt(&project, Some(&agent));
        let scrubbed = scrub(&prompt, &folder);
        assert_snapshot("dev_loop_with_identity", &scrubbed);
    });
}

#[test]
fn snapshot_dev_loop_with_identity_and_agents_md() {
    with_test_command_override_unset(|| {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "Always run cargo check before tests.\nNo emojis.\n",
        )
        .unwrap();
        let folder = dir.path().to_string_lossy().into_owned();
        let project = demo_project(&folder);
        let skills = vec!["Rust".to_string(), "TypeScript".to_string()];
        let agent = AgentInfo {
            identity: Some(AgentIdentity {
                name: "Atlas",
                role: "Engineer",
                personality: "Precise and methodical.",
            }),
            skills: &skills,
            system_prompt: Some("Use TDD on every change."),
        };
        let prompt = agentic_execution_system_prompt(&project, Some(&agent));
        let scrubbed = scrub(&prompt, &folder);
        assert_snapshot("dev_loop_with_identity_and_agents_md", &scrubbed);
    });
}

#[test]
fn snapshot_chat_default() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().to_string_lossy().into_owned();
    let project = demo_project(&folder);
    let prompt = build_chat_system_prompt(&project, "");
    let scrubbed = scrub(&prompt, &folder);
    assert_snapshot("chat_default", &scrubbed);
}

#[test]
fn snapshot_chat_with_agents_md() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "Always run cargo check before tests.\nNo emojis.\n",
    )
    .unwrap();
    let folder = dir.path().to_string_lossy().into_owned();
    let project = demo_project(&folder);
    let prompt = build_chat_system_prompt(&project, "");
    let scrubbed = scrub(&prompt, &folder);
    assert_snapshot("chat_with_agents_md", &scrubbed);
}