use super::*;

fn test_project() -> ProjectInfo<'static> {
    ProjectInfo {
        project_id: None,
        name: "test",
        description: "Test project",
        folder_path: "/tmp/test",
        build_command: Some("cargo build"),
        test_command: Some("cargo test"),
    }
}

fn test_spec(content: &str) -> SpecInfo<'_> {
    SpecInfo {
        title: "Test Spec",
        markdown_contents: content,
    }
}

fn test_task<'a>(title: &'a str, desc: &'a str) -> TaskInfo<'a> {
    TaskInfo {
        title,
        description: desc,
        execution_notes: "",
        files_changed: &[],
    }
}

fn test_session() -> SessionInfo<'static> {
    SessionInfo {
        summary_of_previous_context: "",
    }
}

fn empty_analysis() -> BuildFixPromptData {
    BuildFixPromptData::default()
}

#[test]
fn test_build_fix_prompt_contains_error_output() {
    let project = test_project();
    let spec = test_spec("spec content");
    let task = test_task("Fix build", "Fix the build errors");
    let session = test_session();
    let analysis = empty_analysis();
    let params = BuildFixPromptParams {
        project: &project,
        spec: &spec,
        task: &task,
        session: &session,
        codebase_snapshot: "",
        build_command: "cargo build",
        stderr: "error[E0308]: mismatched types",
        stdout: "Compiling test v0.1.0",
        prior_notes: "initial notes",
        prior_attempts: &[],
        analysis: &analysis,
    };
    let prompt = build_fix_prompt(&params);
    assert!(
        prompt.contains("error[E0308]"),
        "stderr should be in prompt"
    );
    assert!(
        prompt.contains("Compiling test"),
        "stdout should be in prompt"
    );
}

#[test]
fn test_build_fix_prompt_contains_task_and_spec() {
    let project = test_project();
    let spec = test_spec("implement login flow");
    let task = test_task("Add login handler", "Create the login endpoint");
    let session = test_session();
    let analysis = empty_analysis();
    let params = BuildFixPromptParams {
        project: &project,
        spec: &spec,
        task: &task,
        session: &session,
        codebase_snapshot: "",
        build_command: "cargo build",
        stderr: "error: cannot find function",
        stdout: "",
        prior_notes: "",
        prior_attempts: &[],
        analysis: &analysis,
    };
    let prompt = build_fix_prompt(&params);
    assert!(
        prompt.contains("Add login handler"),
        "task title should be in prompt"
    );
    assert!(
        prompt.contains("implement login flow"),
        "spec content should be in prompt"
    );
}

#[test]
fn test_build_fix_prompt_with_history_includes_prior_attempts() {
    let project = test_project();
    let spec = test_spec("spec");
    let task = test_task("Fix it", "Fix");
    let session = test_session();
    let prior = vec![PriorFixAttempt {
        stderr: "first error".into(),
        files_changed: vec!["src/main.rs".into()],
        changes_summary: "changed main".into(),
    }];
    let analysis = empty_analysis();
    let params = BuildFixPromptParams {
        project: &project,
        spec: &spec,
        task: &task,
        session: &session,
        codebase_snapshot: "",
        build_command: "cargo build",
        stderr: "second error",
        stdout: "",
        prior_notes: "",
        prior_attempts: &prior,
        analysis: &analysis,
    };
    let prompt = build_fix_prompt(&params);
    assert!(
        prompt.contains("Previous Fix Attempts"),
        "should mention prior attempts"
    );
    assert!(
        prompt.contains("first error"),
        "prior error should be included"
    );
    assert!(
        prompt.contains("changed main"),
        "prior changes should be included"
    );
}

#[test]
fn test_build_fix_prompt_with_history_empty_prior() {
    let project = test_project();
    let spec = test_spec("spec");
    let task = test_task("Fix", "Fix");
    let session = test_session();
    let analysis = empty_analysis();
    let params = BuildFixPromptParams {
        project: &project,
        spec: &spec,
        task: &task,
        session: &session,
        codebase_snapshot: "",
        build_command: "cargo build",
        stderr: "some error",
        stdout: "",
        prior_notes: "",
        prior_attempts: &[],
        analysis: &analysis,
    };
    let prompt = build_fix_prompt(&params);
    assert!(
        !prompt.contains("Previous Fix Attempts"),
        "no prior section when empty"
    );
}

#[test]
fn test_methods_not_found_warning_renders_when_count_above_threshold() {
    let project = test_project();
    let spec = test_spec("");
    let task = test_task("t", "d");
    let session = test_session();
    let analysis = BuildFixPromptData {
        methods_not_found_count: 4,
        ..BuildFixPromptData::default()
    };
    let params = BuildFixPromptParams {
        project: &project,
        spec: &spec,
        task: &task,
        session: &session,
        codebase_snapshot: "",
        build_command: "cargo build",
        stderr: "x",
        stdout: "",
        prior_notes: "",
        prior_attempts: &[],
        analysis: &analysis,
    };
    let prompt = build_fix_prompt(&params);
    assert!(prompt.contains("calling 3+ methods that do not exist"));
}

#[test]
fn test_truncate_prompt_output_within_limit() {
    let short = "hello world";
    let result = truncate_prompt_output(short, 1000);
    assert_eq!(result, short);
}

#[test]
fn test_truncate_prompt_output_over_limit() {
    let long = "x".repeat(10_000);
    let result = truncate_prompt_output(&long, 200);
    assert!(result.len() < long.len());
    assert!(result.contains("truncated"));
}
