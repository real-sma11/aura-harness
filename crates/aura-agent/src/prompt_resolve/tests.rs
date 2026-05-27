//! Integration tests for the IO half (resolve_hints orchestration +
//! FsWorkspace). The pure extractor + renderer tests live in
//! `aura-prompts/src/enrichment/tests.rs`.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;

/// In-memory [`WorkspaceReader`] stub. Tests configure `files`
/// (path -> contents) and `definitions` (symbol -> hits) and the
/// stub answers from those maps. No real filesystem access.
#[derive(Default)]
struct StubWorkspace {
    files: Mutex<HashMap<String, String>>,
    definitions: Mutex<HashMap<String, Vec<SymbolHit>>>,
    module_paths: Mutex<HashMap<String, Vec<String>>>,
}

impl StubWorkspace {
    fn with_file(self, path: &str, body: &str) -> Self {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), body.to_string());
        self
    }

    fn with_definition(self, symbol: &str, hits: Vec<SymbolHit>) -> Self {
        self.definitions
            .lock()
            .unwrap()
            .insert(symbol.to_string(), hits);
        self
    }

    fn with_module_paths(self, module: &str, paths: Vec<&str>) -> Self {
        self.module_paths.lock().unwrap().insert(
            module.to_string(),
            paths.into_iter().map(str::to_string).collect(),
        );
        self
    }
}

#[async_trait]
impl WorkspaceReader for StubWorkspace {
    async fn exists(&self, relative_path: &str) -> bool {
        self.files.lock().unwrap().contains_key(relative_path)
    }

    async fn read_file_head(&self, relative_path: &str, max_lines: usize) -> Option<String> {
        self.files
            .lock()
            .unwrap()
            .get(relative_path)
            .map(|body| body.lines().take(max_lines).collect::<Vec<_>>().join("\n"))
    }

    async fn grep_definition(&self, symbol: &str, max_hits: usize) -> Vec<SymbolHit> {
        self.definitions
            .lock()
            .unwrap()
            .get(symbol)
            .cloned()
            .map(|v| v.into_iter().take(max_hits).collect())
            .unwrap_or_default()
    }

    async fn discover_module_paths(&self, module: &str, max_hits: usize) -> Vec<String> {
        self.module_paths
            .lock()
            .unwrap()
            .get(module)
            .cloned()
            .map(|v| v.into_iter().take(max_hits).collect())
            .unwrap_or_default()
    }
}

#[tokio::test]
async fn resolve_hints_with_stub_workspace_emits_block() {
    let workspace = StubWorkspace::default()
        .with_file(
            "crates/zero-storage/src/outbox.rs",
            "use crate::prelude::*;\n\npub struct Outbox {\n    inner: Inner,\n}\n",
        )
        .with_definition(
            "Outbox::enqueue",
            vec![SymbolHit {
                path: "crates/zero-storage/src/outbox.rs".into(),
                line: 84,
                text: "pub fn enqueue(&mut self, item: Item) {".into(),
            }],
        );
    let hints = ContextHints {
        paths: vec!["crates/zero-storage/src/outbox.rs".into()],
        symbols: vec!["Outbox::enqueue".into()],
        ..Default::default()
    };
    let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
    assert!(!resolved.is_empty());
    let block = resolved.into_block();
    assert!(block.contains("## Pre-resolved context"));
    assert!(block.contains("crates/zero-storage/src/outbox.rs"));
    assert!(block.contains("pub struct Outbox"));
    assert!(block.contains("Outbox::enqueue"));
    assert!(block.contains("outbox.rs:84"));
    assert!(block.contains("starting points"));
}

#[tokio::test]
async fn resolve_hints_discovers_module_reference_heads() {
    let workspace = StubWorkspace::default()
        .with_file(
            "crates/zero-storage/src/inbox.rs",
            "pub struct InboxEntry;\nimpl InboxEntry {}\n",
        )
        .with_file(
            "crates/zero-storage/src/storage.rs",
            "pub const CF_INBOX: &str = \"inbox\";\npub const CF_OUTBOX: &str = \"outbox\";\n",
        )
        .with_module_paths("inbox", vec!["crates/zero-storage/src/inbox.rs"])
        .with_module_paths("storage", vec!["crates/zero-storage/src/storage.rs"]);
    let hints = extract_hints("2.6 outbox CF");

    let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
    let block = resolved.into_block();

    assert!(block.contains("crates/zero-storage/src/inbox.rs"));
    assert!(block.contains("pub struct InboxEntry"));
    assert!(block.contains("crates/zero-storage/src/storage.rs"));
    assert!(block.contains("CF_OUTBOX"));
    assert!(block.contains("outbox.rs"));
}

#[tokio::test]
async fn resolve_hints_skips_missing_files_silently() {
    let workspace = StubWorkspace::default()
        .with_file("crates/zero-storage/src/outbox.rs", "pub struct Outbox;");
    let hints = ContextHints {
        paths: vec![
            "crates/zero-storage/src/outbox.rs".into(),
            "crates/imaginary/src/ghost.rs".into(),
        ],
        symbols: vec![],
        ..Default::default()
    };
    let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
    let block = resolved.into_block();
    assert!(block.contains("crates/zero-storage/src/outbox.rs"));
    assert!(!block.contains("ghost.rs"));
}

#[tokio::test]
async fn resolve_hints_empty_block_for_no_resolutions() {
    let workspace = StubWorkspace::default();
    let hints = ContextHints {
        paths: vec!["crates/nope/src/missing.rs".into()],
        symbols: vec!["NotAThing".into()],
        ..Default::default()
    };
    let resolved = resolve_hints(&hints, &workspace, default_caps()).await;
    assert!(resolved.is_empty());
    assert_eq!(resolved.into_block(), "");
}

#[tokio::test]
async fn resolve_hints_honours_max_block_chars_by_dropping_bodies() {
    let big_body = "fn line() {}\n".repeat(200);
    let workspace = StubWorkspace::default()
        .with_file("crates/a/src/lib.rs", &big_body)
        .with_file("crates/b/src/lib.rs", &big_body);
    let hints = ContextHints {
        paths: vec!["crates/a/src/lib.rs".into(), "crates/b/src/lib.rs".into()],
        symbols: vec![],
        ..Default::default()
    };
    let caps = ResolveCaps {
        max_block_chars: 400,
        ..default_caps()
    };
    let resolved = resolve_hints(&hints, &workspace, caps).await;
    let block = resolved.into_block();
    assert!(block.contains("crates/a/src/lib.rs"));
    assert!(block.contains("crates/b/src/lib.rs"));
    assert!(
        block.len() <= 1000,
        "expected block trimmed near budget, got {} chars",
        block.len()
    );
}

#[tokio::test]
async fn fs_workspace_reads_real_file_head() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hello.rs"),
        "line1\nline2\nline3\nline4\nline5\n",
    )
    .unwrap();
    let ws = FsWorkspace::new(dir.path());
    assert!(ws.exists("hello.rs").await);
    assert!(!ws.exists("missing.rs").await);
    let head = ws.read_file_head("hello.rs", 3).await.unwrap();
    assert_eq!(head, "line1\nline2\nline3");
}

#[tokio::test]
async fn fs_workspace_grep_definition_finds_pub_fn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub struct Outbox;\n\nimpl Outbox {\n    pub fn enqueue(&self) {}\n}\n",
    )
    .unwrap();
    let ws = FsWorkspace::new(dir.path());
    let hits = ws.grep_definition("Outbox::enqueue", 3).await;
    assert!(!hits.is_empty(), "expected at least one hit");
    let first = &hits[0];
    assert_eq!(first.path, "src/lib.rs");
    assert!(first.text.contains("fn enqueue"));
}
