use super::*;

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
fn fix_system_prompt_contains_json_instructions() {
    let prompt = build_fix_system_prompt();
    assert!(prompt.contains("valid JSON object"));
    assert!(prompt.contains("search_replace"));
}

#[test]
fn agentic_prompt_includes_build_command() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);
    assert!(prompt.contains("cargo build"));
    assert!(prompt.contains("cargo test"));
}

#[test]
fn agentic_prompt_includes_agent_preamble() {
    let project = test_project("/nonexistent");
    let skills = vec!["Rust".to_string(), "Python".to_string()];
    let agent = AgentInfo {
        name: "TestAgent",
        role: "backend engineer",
        personality: "Precise and methodical.",
        system_prompt: "",
        skills: &skills,
    };
    let prompt = agentic_execution_system_prompt(&project, Some(&agent), None, 20);
    assert!(prompt.contains("TestAgent"));
    assert!(prompt.contains("backend engineer"));
    assert!(prompt.contains("Precise and methodical."));
    assert!(prompt.contains("Rust, Python"));
}

#[test]
fn agentic_prompt_includes_workspace_context() {
    let project = test_project("/nonexistent");
    let prompt =
        agentic_execution_system_prompt(&project, None, Some("Contains 5 crate members"), 20);
    assert!(prompt.contains("Workspace Context"));
    assert!(prompt.contains("5 crate members"));
}

#[test]
fn agentic_prompt_includes_definition_of_done_hard_gate() {
    // After the 2026-05 strip, the verbose "DEFINITION OF DONE (HARD
    // GATE)" block was inlined into a single rule. The contract is
    // unchanged — the agent must not call task_done while the build
    // is broken or the test suite is failing — so the assertions
    // pin the shorter rendering instead of the old section header.
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);
    assert!(
        prompt.contains("hard gate"),
        "task_done hard-gate language missing: {prompt}"
    );
    assert!(
        prompt.contains("Do not call `task_done`"),
        "task_done deferral instruction missing: {prompt}"
    );
    assert!(
        prompt.contains("test suite"),
        "test-suite reference missing: {prompt}"
    );
}

#[test]
fn agentic_prompt_no_longer_tells_agent_to_ignore_pre_existing_failures() {
    // Regression guard: this exact phrasing previously instructed
    // agents to skip pre-existing failures, which contradicts the
    // hard gate.
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);
    assert!(
        !prompt.contains("IGNORE them"),
        "system prompt still tells agent to IGNORE pre-existing failures"
    );
    assert!(
        !prompt.contains("If they are pre-existing and unrelated to your changes"),
        "system prompt still contains the old pre-existing-failure escape hatch"
    );
}

/// When the operator has set `AURA_DOD_TEST_COMMAND`, the prompt's
/// rendered test command must match the override so the agent sees the
/// exact command the gate is going to run. Otherwise the agent would
/// keep mentally validating against the project default while the gate
/// silently runs something else.
///
/// We mutate the global env in-test, which can race other tests reading
/// the same var. Save/restore around the assertion keeps it isolated.
#[test]
fn agentic_prompt_uses_test_command_env_override_when_set() {
    use crate::task_executor::TEST_COMMAND_OVERRIDE_ENV;
    let prev = std::env::var(TEST_COMMAND_OVERRIDE_ENV).ok();

    std::env::set_var(TEST_COMMAND_OVERRIDE_ENV, "pytest -q -k smoke");

    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);
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
    assert!(prompt.starts_with(CHAT_SYSTEM_PROMPT_BASE));
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
    assert!(prompt.contains("(not set)"));
}

#[test]
fn agentic_prompt_includes_tool_call_discipline_section() {
    let project = test_project("/nonexistent");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);

    assert!(
        prompt.contains("Tool-call discipline:"),
        "discipline section header missing from assembled prompt"
    );
    assert!(
        prompt.contains("write_file must stay at or under 32000 bytes"),
        "write_file chunk rule missing"
    );
    assert!(
        prompt.contains(
            "After any read_file or search_code call, your next turn must either call \
             another tool or submit a tool_result-producing action"
        ),
        "post-read tool-call rule missing"
    );
    assert!(
        prompt
            .contains("Never issue two search_code calls whose patterns share an alternation term"),
        "overlapping-alternation rule missing"
    );
    assert!(
        prompt.contains("If your last two turns produced no tool calls, the next turn MUST be a single tool call"),
        "two-turns-no-tool rule missing"
    );
    assert!(
        prompt.contains("fields removed and replaced with `_redacted` metadata"),
        "write/edit redaction metadata rule missing"
    );
}

#[test]
fn tool_call_discipline_constant_matches_golden_wording() {
    assert!(TOOL_CALL_DISCIPLINE_SECTION.starts_with("Tool-call discipline:\n"));
    assert!(TOOL_CALL_DISCIPLINE_SECTION.contains("32000 bytes per call"));
    assert!(TOOL_CALL_DISCIPLINE_SECTION.contains("append_after_eof"));
    assert!(TOOL_CALL_DISCIPLINE_SECTION.contains("alternation term"));
    assert!(TOOL_CALL_DISCIPLINE_SECTION.contains("MUST be a single tool call"));
    assert!(TOOL_CALL_DISCIPLINE_SECTION.contains("_redacted"));
}

#[test]
fn chat_system_prompt_detects_tech_stack() {
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
    assert!(prompt.contains("Rust"));
    assert!(prompt.contains("Node.js/TypeScript"));
}

// ---------------------------------------------------------------------------
// AGENTS.md injection
//
// `append_agents_md` runs from BOTH `build_chat_system_prompt` and
// `agentic_execution_system_prompt`, so we mirror the assertions across
// both builders to lock the contract in place. The helper is internal
// (private), so tests assert against the rendered prompt instead of
// calling it directly.
// ---------------------------------------------------------------------------

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
        prompt.contains(AGENTS_MD_SECTION_HEADER),
        "chat prompt is missing the AGENTS.md section header"
    );
    assert!(
        prompt.contains("Always run cargo check before tests."),
        "chat prompt did not include AGENTS.md body"
    );
    assert!(
        prompt.contains("`AGENTS.md`"),
        "chat prompt did not mention the matched filename variant"
    );
}

#[test]
fn chat_system_prompt_handles_case_insensitive_variants() {
    let dir = tempfile::tempdir().unwrap();
    // Only the lowercase variant exists.
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

    assert!(prompt.contains(AGENTS_MD_SECTION_HEADER));
    assert!(prompt.contains("Lowercase variant body."));
    // On case-insensitive filesystems (Windows / macOS default) the
    // first probe `AGENTS.md` opens the same inode as `agents.md`, so
    // the variant label may be either. On case-sensitive filesystems
    // (Linux) only the second probe matches and the label is
    // `agents.md`. Accept both so the test is cross-platform.
    assert!(
        prompt.contains("`AGENTS.md`") || prompt.contains("`agents.md`"),
        "expected one of the recognised variant labels in the rendered prompt"
    );
}

#[test]
fn chat_system_prompt_omits_agents_md_when_absent() {
    let dir = tempfile::tempdir().unwrap();
    // Intentionally do not write AGENTS.md.
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
        !prompt.contains(AGENTS_MD_SECTION_HEADER),
        "chat prompt unexpectedly includes the AGENTS.md section when no file is present"
    );
}

#[test]
fn chat_system_prompt_skips_agents_md_when_over_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    // 1 byte over the cap so the helper's size guard trips.
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
        !prompt.contains(AGENTS_MD_SECTION_HEADER),
        "oversize AGENTS.md must be skipped, not truncated"
    );
    // And the giant payload must not leak into the prompt either.
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
        !prompt.contains(AGENTS_MD_SECTION_HEADER),
        "non-existent folder_path must not surface an AGENTS.md section"
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
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);

    assert!(prompt.contains(AGENTS_MD_SECTION_HEADER));
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
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_HEADER),
        "agentic prompt unexpectedly includes the AGENTS.md section when no file is present"
    );
}

#[test]
fn agentic_prompt_skips_agents_md_when_folder_missing() {
    let project = test_project("/definitely/does/not/exist/aura/agentic/test");
    let prompt = agentic_execution_system_prompt(&project, None, None, 20);

    assert!(
        !prompt.contains(AGENTS_MD_SECTION_HEADER),
        "non-existent folder_path must not surface an AGENTS.md section in the agentic prompt"
    );
}
