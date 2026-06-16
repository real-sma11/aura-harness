use super::*;
use tempfile::TempDir;

fn create_test_sandbox() -> (Sandbox, TempDir) {
    let dir = TempDir::new().unwrap();
    let sandbox = Sandbox::new(dir.path()).unwrap();
    (sandbox, dir)
}

#[test]
fn test_search_code_simple_pattern() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(
        dir.path().join("code.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();

    let result = search_code(&sandbox, "fn main", None, None, 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("fn main"));
    assert!(output.contains("code.rs"));
}

#[test]
fn test_search_code_regex_pattern() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("code.rs"), "let x = 42;\nlet y = 123;").unwrap();

    let result = search_code(&sandbox, r"let \w+ = \d+", None, None, 100, 0).unwrap();
    assert!(result.ok);
    assert_eq!(result.metadata.get("match_count").unwrap(), "2");
}

#[test]
fn test_search_code_no_matches() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("code.rs"), "fn main() {}").unwrap();

    let result = search_code(&sandbox, "nonexistent_pattern_xyz", None, None, 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("No matches found"));
}

#[test]
fn test_search_code_file_pattern() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("code.rs"), "let rust_var = 1;").unwrap();
    fs::write(dir.path().join("code.ts"), "let ts_var = 2;").unwrap();

    let result = search_code(&sandbox, "let", None, Some("*.rs"), 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("rust_var"));
    assert!(!output.contains("ts_var"));
}

#[test]
fn test_search_code_max_results() {
    let (sandbox, dir) = create_test_sandbox();

    let content = (0..20)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.path().join("many.txt"), content).unwrap();

    let result = search_code(&sandbox, "line", None, None, 5, 0).unwrap();
    assert!(result.ok);
    assert_eq!(result.metadata.get("match_count").unwrap(), "5");
}

#[test]
fn test_search_code_in_subdirectory() {
    let (sandbox, dir) = create_test_sandbox();

    fs::create_dir_all(dir.path().join("src/nested")).unwrap();
    fs::write(dir.path().join("src/nested/code.rs"), "fn nested_fn() {}").unwrap();

    let result = search_code(&sandbox, "nested_fn", Some("src"), None, 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("nested_fn"));
}

#[test]
fn test_search_code_invalid_regex() {
    let (sandbox, _dir) = create_test_sandbox();

    let result = search_code(&sandbox, "[invalid(regex", None, None, 100, 0);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_search_code_context_lines() {
    let (sandbox, dir) = create_test_sandbox();

    let content = "alpha\nbeta\ngamma\ndelta\nepsilon\n";
    fs::write(dir.path().join("ctx.txt"), content).unwrap();

    let result = search_code(&sandbox, "gamma", None, None, 100, 1).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("beta"));
    assert!(output.contains("gamma"));
    assert!(output.contains("delta"));
    assert!(output.contains(">"));
}

#[test]
fn test_search_code_skip_dirs() {
    let (sandbox, dir) = create_test_sandbox();

    fs::create_dir_all(dir.path().join("node_modules")).unwrap();
    fs::write(dir.path().join("node_modules/dep.js"), "let hidden = true;").unwrap();
    fs::create_dir_all(dir.path().join("target")).unwrap();
    fs::write(dir.path().join("target/out.rs"), "let hidden = true;").unwrap();
    fs::write(dir.path().join("visible.rs"), "let visible = true;").unwrap();

    let result = search_code(&sandbox, "let", None, None, 100, 0).unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("visible"));
    assert!(!output.contains("hidden"));
}

#[test]
fn test_search_code_complex_regex_lookahead_character_class() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(
        dir.path().join("complex.rs"),
        "fn foo_bar() {}\nfn baz123() {}\nfn _private() {}\n",
    )
    .unwrap();

    let result = search_code(&sandbox, r"fn [a-z_]+\d*\(\)", None, None, 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("baz123"));
}

#[test]
fn test_search_code_alternation_regex() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(
        dir.path().join("alt.rs"),
        "let alpha = 1;\nlet beta = 2;\nlet gamma = 3;\n",
    )
    .unwrap();

    let result = search_code(&sandbox, r"alpha|gamma", None, None, 100, 0).unwrap();
    assert_eq!(result.metadata.get("match_count").unwrap(), "2");
}

#[test]
fn test_search_code_binary_file_skipped() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(
        dir.path().join("image.png"),
        b"fake png data with let x = 1",
    )
    .unwrap();
    fs::write(dir.path().join("code.rs"), "let x = 1;").unwrap();

    let result = search_code(&sandbox, "let x", None, None, 100, 0).unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("code.rs"));
    assert!(!output.contains("image.png"));
}

#[test]
fn test_search_code_nonexistent_path_diagnostic() {
    let (sandbox, _dir) = create_test_sandbox();

    let result = search_code(&sandbox, "anything", Some("no_such_dir"), None, 100, 0);
    assert!(result.is_err());
}

#[test]
fn test_search_code_zero_match_regex_hint() {
    let (sandbox, dir) = create_test_sandbox();

    fs::write(dir.path().join("hint.rs"), "normal code").unwrap();

    let result = search_code(&sandbox, r"foo\(bar\[baz\]", None, None, 100, 0).unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("No matches found"));
    assert!(output.contains("regex characters"));
}

#[test]
fn test_search_code_regex_size_limit() {
    let (sandbox, _dir) = create_test_sandbox();

    let huge_pattern = "a".repeat(SEARCH_REGEX_SIZE_LIMIT + 1);
    let result = search_code(&sandbox, &huge_pattern, None, None, 100, 0);
    assert!(matches!(result, Err(ToolError::InvalidArguments(_))));
}

#[test]
fn test_search_code_context_lines_clamped_to_10() {
    let (sandbox, dir) = create_test_sandbox();

    let content = (0..30)
        .map(|i| format!("line_{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.path().join("ctx.txt"), &content).unwrap();

    let result = search_code(&sandbox, "line_15", None, None, 100, 100).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("line_15"));
}

#[test]
fn test_is_text_file_known_extensions() {
    use std::path::Path;
    assert!(is_text_file(Path::new("main.rs")));
    assert!(is_text_file(Path::new("script.py")));
    assert!(is_text_file(Path::new("config.json")));
    assert!(is_text_file(Path::new("readme.md")));
    assert!(!is_text_file(Path::new("photo.jpg")));
    assert!(!is_text_file(Path::new("binary.exe")));
}

// Repro for the silent-truncation bug: when results hit `max_results`, the
// caller is given no signal that matches were dropped, so a capped result is
// treated as exhaustive (false-negatives). A capped search MUST flag itself.
#[test]
fn test_search_code_signals_truncation_when_results_capped() {
    let (sandbox, dir) = create_test_sandbox();

    // 20 matching lines, cap at 5 -> 15 matches silently dropped.
    let content = (0..20)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(dir.path().join("many.txt"), content).unwrap();

    let result = search_code(&sandbox, "line", None, None, 5, 0).unwrap();
    assert!(result.ok);
    // The cap is enforced (existing behavior)...
    assert_eq!(result.metadata.get("match_count").unwrap(), "5");
    // ...but the caller MUST be told results were dropped.
    assert_eq!(
        result.metadata.get("truncated").map(String::as_str),
        Some("true"),
        "capped search must set truncated=\"true\" metadata"
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(
        output.to_lowercase().contains("truncat"),
        "capped search output must include a truncation marker; got:\n{output}"
    );
}

// Guard against a spurious marker: a search that does NOT hit the cap must
// not claim truncation.
#[test]
fn test_search_code_no_truncation_signal_when_under_cap() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(dir.path().join("few.txt"), "line0\nline1\nline2").unwrap();

    let result = search_code(&sandbox, "line", None, None, 100, 0).unwrap();
    assert!(result.ok);
    assert_eq!(result.metadata.get("match_count").unwrap(), "3");
    assert_eq!(result.metadata.get("truncated"), None);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(!output.to_lowercase().contains("truncat"));
}

// search_code must not silently skip source files whose extension isn't on a
// hardcoded text allowlist — the old behavior dropped .tsx/.jsx/.vue/.scss etc.,
// which are most of a typical web codebase (and caused real false-negatives).
#[test]
fn test_search_code_finds_unlisted_text_extensions() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(dir.path().join("Component.tsx"), "const a = needle;").unwrap();
    fs::write(dir.path().join("widget.jsx"), "const b = needle;").unwrap();
    fs::write(dir.path().join("styles.scss"), "/* needle */").unwrap();
    fs::write(dir.path().join("App.vue"), "<!-- needle -->").unwrap();

    let result = search_code(&sandbox, "needle", None, None, 100, 0).unwrap();
    assert!(result.ok);
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("Component.tsx"), "missed .tsx:\n{output}");
    assert!(output.contains("widget.jsx"), "missed .jsx:\n{output}");
    assert!(output.contains("styles.scss"), "missed .scss:\n{output}");
    assert!(output.contains("App.vue"), "missed .vue:\n{output}");
}

// Binary files must still be skipped (denylist + the UTF-8 read backstop).
#[test]
fn test_search_code_still_skips_binary_files() {
    let (sandbox, dir) = create_test_sandbox();
    fs::write(dir.path().join("img.png"), "needle in png").unwrap();
    fs::write(dir.path().join("font.woff2"), "needle in font").unwrap();
    fs::write(dir.path().join("real.ts"), "const c = needle;").unwrap();

    let result = search_code(&sandbox, "needle", None, None, 100, 0).unwrap();
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("real.ts"));
    assert!(!output.contains("img.png"), "should skip .png");
    assert!(!output.contains("font.woff2"), "should skip .woff2");
}

// Classification is denylist-based and case-insensitive.
#[test]
fn test_is_text_file_denylist_and_case_insensitive() {
    use std::path::Path;
    assert!(is_text_file(Path::new("Component.tsx")));
    assert!(is_text_file(Path::new("widget.jsx")));
    assert!(is_text_file(Path::new("Main.RS"))); // uppercase source still text
    assert!(!is_text_file(Path::new("photo.PNG"))); // uppercase binary still skipped
    assert!(!is_text_file(Path::new("lib.dylib")));
}
