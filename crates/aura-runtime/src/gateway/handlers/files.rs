use super::super::*;
use crate::config::PathError;
use crate::files_api::{self, ReadOutcome, WalkedEntry, MAX_READ_BYTES, MAX_WALK_DEPTH};
use base64::Engine as _;
use std::path::{Component, PathBuf};

const MAX_IMPORTED_FILES: usize = 4_096;
const MAX_IMPORTED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(in crate::gateway) struct ListFilesQuery {
    #[serde(default = "default_files_path")]
    path: String,
    #[serde(default = "default_files_depth")]
    depth: usize,
}

fn default_files_path() -> String {
    ".".into()
}
fn default_files_depth() -> usize {
    3
}

/// Wire shape for a single directory entry on `/api/files`.
///
/// Paths are workspace-relative and forward-slash normalised so the
/// frontend can render them unchanged on Windows and Unix. That
/// contract is the reason this DTO is owned by the router rather than
/// [`crate::files_api`] — the file-API helper stays on raw absolute
/// paths and each caller (aura-runtime vs the TUI embedded server)
/// handles its own serialisation shape.
#[derive(Debug, Serialize)]
struct FileDirEntry {
    name: String,
    path: String,
    is_dir: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    children: Option<Vec<Self>>,
}

/// Convert walker output into [`FileDirEntry`] tree with workspace-relative
/// forward-slash paths. `base` is the directory the caller started
/// the walk from (after `resolve_allowed_path`); relative paths are
/// reported relative to `base` so the frontend can navigate within
/// the listing without knowing the absolute sandbox root.
fn to_file_entries(base: &std::path::Path, entries: Vec<WalkedEntry>) -> Vec<FileDirEntry> {
    entries
        .into_iter()
        .map(|e| {
            let rel = e
                .abs_path
                .strip_prefix(base)
                .unwrap_or(&e.abs_path)
                .to_string_lossy()
                .into_owned()
                .replace('\\', "/");
            FileDirEntry {
                name: e.name,
                path: rel,
                is_dir: e.is_dir,
                children: e.children.map(|c| to_file_entries(base, c)),
            }
        })
        .collect()
}

/// Map a [`PathError`] to an HTTP status + JSON body.
///
/// Keeping the mapping in one place means `/api/files` and
/// `/api/read-file` report traversal attempts, missing files, and
/// permission failures with the same status codes, and changes to that
/// policy only need to land here.
fn path_error_response(err: &PathError) -> (StatusCode, Json<serde_json::Value>) {
    match err {
        PathError::NotFound(p) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("path not found: {}", p.display()),
            })),
        ),
        PathError::Escapes(_) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "ok": false,
                "error": "path escapes workspace",
            })),
        ),
        PathError::NotPermitted(msg) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": msg,
            })),
        ),
    }
}

pub(in crate::gateway) async fn list_files_handler(
    State(state): State<RouterState>,
    Query(query): Query<ListFilesQuery>,
) -> impl IntoResponse {
    let depth = query.depth.min(MAX_WALK_DEPTH);

    // Every path — including the default "." — goes through the
    // canonicalizing resolver so a caller can't sneak past with e.g.
    // "./../etc". When the resolved path isn't a directory we surface
    // 400 rather than silently walking a single file.
    let input = std::path::Path::new(&query.path);
    let base = match state.config.resolve_allowed_path(input) {
        Ok(p) => p,
        Err(e) => {
            let (status, body) = path_error_response(&e);
            return (status, body).into_response();
        }
    };

    match tokio::fs::metadata(&base).await {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "path is not a directory" })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": "path not found" })),
            )
                .into_response();
        }
    }

    // `files_api::walk_directory` returns absolute `PathBuf`s; we
    // render them workspace-relative with forward slashes via
    // `to_file_entries` so the JSON contract stays identical to the
    // pre-consolidation handler.
    let walked = files_api::walk_directory(&base, None, depth).await;
    let entries = to_file_entries(&base, walked);

    (
        StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "entries": entries })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub(in crate::gateway) struct ReadFileQuery {
    path: String,
}

pub(in crate::gateway) async fn read_file_handler(
    State(state): State<RouterState>,
    Query(query): Query<ReadFileQuery>,
) -> impl IntoResponse {
    let input = std::path::Path::new(&query.path);
    let resolved = match state.config.resolve_allowed_path(input) {
        Ok(p) => p,
        Err(e) => {
            let (status, body) = path_error_response(&e);
            return (status, body).into_response();
        }
    };

    // Reject directories explicitly so we don't end up returning
    // `read_to_end` of a directory (which is an OS-specific error on
    // Linux / empty on Windows).
    match tokio::fs::metadata(&resolved).await {
        Ok(m) if m.is_file() => {}
        Ok(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": "path is not a file" })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "ok": false, "error": "path not found" })),
            )
                .into_response();
        }
    }

    match files_api::read_file_capped(&resolved, MAX_READ_BYTES).await {
        Ok(ReadOutcome::Ok { bytes }) => {
            // Decode as UTF-8 for the JSON payload. Lossy conversion
            // matches the previous `read_to_string` behaviour for
            // clean text files and degrades gracefully on binary
            // input instead of returning a 500.
            let content = String::from_utf8_lossy(&bytes).into_owned();
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "ok": true,
                    "content": content,
                    "path": resolved.to_string_lossy(),
                    "bytes": bytes.len(),
                })),
            )
                .into_response()
        }
        Ok(ReadOutcome::TooLarge { max_bytes }) => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("file exceeds {max_bytes}-byte read cap"),
                "max_bytes": max_bytes,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("{e}: {}", resolved.display()),
            })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(in crate::gateway) struct ResolveWorkspaceQuery {
    project_name: String,
}

pub(in crate::gateway) async fn resolve_workspace_handler(
    State(state): State<RouterState>,
    Query(query): Query<ResolveWorkspaceQuery>,
) -> impl IntoResponse {
    let path = state
        .config
        .resolve_workspace_for_project(&query.project_name);
    Json(serde_json::json!({
        "path": path.to_string_lossy(),
    }))
}

#[derive(Debug, Deserialize)]
pub(in crate::gateway) struct ImportWorkspaceRequest {
    workspace_key: String,
    files: Vec<ImportedWorkspaceFile>,
}

#[derive(Debug, Deserialize)]
struct ImportedWorkspaceFile {
    relative_path: String,
    contents_base64: String,
}

fn validate_workspace_key(workspace_key: &str) -> Result<&str, String> {
    let workspace_key = workspace_key.trim();
    if workspace_key.is_empty() {
        return Err("workspace_key is required".to_string());
    }
    if workspace_key.len() > 128
        || !workspace_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(
            "workspace_key must contain only ASCII letters, numbers, '-' or '_'".to_string(),
        );
    }
    Ok(workspace_key)
}

fn sanitize_import_path(relative_path: &str) -> Result<PathBuf, String> {
    let mut sanitized = PathBuf::new();
    for component in std::path::Path::new(relative_path).components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("invalid imported file path: {relative_path}"));
            }
        }
    }
    if sanitized.as_os_str().is_empty() {
        return Err("imported files must include a relative path".to_string());
    }
    Ok(sanitized)
}

async fn create_safe_parent(
    workspace: &std::path::Path,
    relative: &std::path::Path,
) -> Result<(), String> {
    let mut current = workspace.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(part) = component else {
                return Err("invalid imported file parent".to_string());
            };
            current.push(part);
            match tokio::fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "imported file parent must not be a symlink: {}",
                        current.display()
                    ));
                }
                Ok(metadata) if !metadata.is_dir() => {
                    return Err(format!(
                        "imported file parent is not a directory: {}",
                        current.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::fs::create_dir(&current).await.map_err(|error| {
                        format!("failed to create imported workspace directory: {error}")
                    })?;
                }
                Err(error) => {
                    return Err(format!(
                        "failed to inspect imported workspace directory: {error}"
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(in crate::gateway) async fn import_workspace_handler(
    State(state): State<RouterState>,
    Json(request): Json<ImportWorkspaceRequest>,
) -> impl IntoResponse {
    let workspace_key = match validate_workspace_key(&request.workspace_key) {
        Ok(workspace_key) => workspace_key,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": error })),
            )
                .into_response();
        }
    };
    if request.files.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "at least one file is required",
            })),
        )
            .into_response();
    }
    if request.files.len() > MAX_IMPORTED_FILES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("import exceeds the {MAX_IMPORTED_FILES}-file limit"),
            })),
        )
            .into_response();
    }

    let file_count = request.files.len();
    let mut decoded = Vec::with_capacity(file_count);
    let mut total_bytes = 0usize;
    for file in request.files {
        let relative_path = match sanitize_import_path(&file.relative_path) {
            Ok(path) => path,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "ok": false, "error": error })),
                )
                    .into_response();
            }
        };
        let contents = match base64::engine::general_purpose::STANDARD.decode(file.contents_base64)
        {
            Ok(contents) => contents,
            Err(error) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("invalid imported file contents: {error}"),
                    })),
                )
                    .into_response();
            }
        };
        total_bytes = match total_bytes.checked_add(contents.len()) {
            Some(total) if total <= MAX_IMPORTED_BYTES => total,
            _ => {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("import exceeds the {MAX_IMPORTED_BYTES}-byte limit"),
                    })),
                )
                    .into_response();
            }
        };
        decoded.push((relative_path, contents));
    }

    let workspace = state.config.resolve_workspace_for_project(workspace_key);
    if let Err(error) = tokio::fs::create_dir_all(&workspace).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("failed to create hosted workspace: {error}"),
            })),
        )
            .into_response();
    }
    match tokio::fs::symlink_metadata(&workspace).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": "hosted workspace root must be a real directory",
                })),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("failed to inspect hosted workspace: {error}"),
                })),
            )
                .into_response();
        }
    }

    for (relative_path, contents) in decoded {
        if let Err(error) = create_safe_parent(&workspace, &relative_path).await {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": error })),
            )
                .into_response();
        }
        let destination = workspace.join(&relative_path);
        match tokio::fs::symlink_metadata(&destination).await {
            Ok(metadata) if metadata.file_type().is_symlink() || metadata.is_dir() => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!(
                            "imported file destination is not a regular file: {}",
                            destination.display()
                        ),
                    })),
                )
                    .into_response();
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "ok": false,
                        "error": format!("failed to inspect import destination: {error}"),
                    })),
                )
                    .into_response();
            }
        }
        if let Err(error) = tokio::fs::write(&destination, contents).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("failed to write imported file: {error}"),
                })),
            )
                .into_response();
        }
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "path": workspace.to_string_lossy(),
            "files": file_count,
            "bytes": total_bytes,
        })),
    )
        .into_response()
}

pub(in crate::gateway) async fn delete_workspace_handler(
    State(state): State<RouterState>,
    Path(workspace_key): Path<String>,
) -> impl IntoResponse {
    let workspace_key = match validate_workspace_key(&workspace_key) {
        Ok(workspace_key) => workspace_key,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    let workspace = state.config.resolve_workspace_for_project(workspace_key);
    match tokio::fs::symlink_metadata(&workspace).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            StatusCode::BAD_REQUEST
        }
        Ok(_) => match tokio::fs::remove_dir_all(&workspace).await {
            Ok(()) => StatusCode::NO_CONTENT,
            Err(error) => {
                warn!(path = %workspace.display(), %error, "failed to delete hosted workspace");
                StatusCode::INTERNAL_SERVER_ERROR
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => StatusCode::NO_CONTENT,
        Err(error) => {
            warn!(path = %workspace.display(), %error, "failed to inspect hosted workspace");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
