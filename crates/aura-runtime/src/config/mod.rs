//! Node configuration.

use std::path::{Path, PathBuf};

/// Default test-only placeholder for [`NodeConfig::auth_token`].
///
/// Real deployments replace this via [`NodeConfig::from_env`] (if
/// `AURA_NODE_AUTH_TOKEN` is set) or [`resolve_auth_token`] (called
/// from `Node::run`). Keeping the placeholder short and well-known
/// means router unit/integration tests can use `Authorization: Bearer
/// test` without any extra wiring. It is **never** the token used in
/// production.
const DEFAULT_TEST_AUTH_TOKEN: &str = "test";

/// Loopback aura-os-server URL the harness defaults to when its
/// listener is bound to loopback and no explicit `AURA_OS_SERVER_URL`
/// is set. Mirrors `PREFERRED_PORT` in
/// `aura-os-desktop/src/net/server.rs`, which is the port the desktop
/// always tries first for its embedded aura-os-server. Kept as a
/// module-private constant so the auto-default in [`NodeConfig::from_env`]
/// and the unit tests pinning that behavior reference the same value.
const DESKTOP_LOOPBACK_OS_SERVER_URL: &str = "http://127.0.0.1:19847";

/// Errors returned by [`NodeConfig::resolve_allowed_path`].
///
/// The variants map onto distinct HTTP statuses so the file handlers can
/// signal `400`, `403`, and `404` separately instead of collapsing every
/// refusal into a single opaque error (which is what the previous
/// `bool`-returning `is_allowed_path` forced them to do).
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// The resolved path does not exist on disk.
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    /// The resolved path's canonical form escapes the workspace root.
    #[error("path escapes workspace: {0}")]
    Escapes(PathBuf),
    /// The workspace root itself is unavailable, or canonicalization
    /// failed for a reason other than `NotFound` (e.g. permission denied).
    #[error("path not permitted: {0}")]
    NotPermitted(String),
}

/// Node configuration.
///
/// Note: `Debug` is implemented manually so `auth_token` is never
/// printed verbatim — if the struct is ever logged via `{:?}` the
/// secret is redacted. Tests and debuggers therefore cannot leak the
/// token through routine tracing.
#[derive(Clone)]
pub struct NodeConfig {
    /// Data directory for `RocksDB` and workspaces
    pub data_dir: PathBuf,
    /// Base directory for project workspaces on remote VMs.
    /// When set (e.g. `/home/aura`), incoming `project_path` / `workspace_root`
    /// values are remapped to `{project_base}/{slug}` where slug is the last
    /// path component of the incoming path.  When `None` paths pass through
    /// unchanged (local development).
    pub project_base: Option<PathBuf>,
    /// HTTP server bind address
    pub bind_addr: String,
    /// Enable sync writes to `RocksDB`
    pub sync_writes: bool,
    /// Record window size for kernel context
    pub record_window_size: usize,
    /// Orbit service URL
    pub orbit_url: String,
    /// Aura Storage service URL
    pub aura_storage_url: String,
    /// Aura Network service URL
    pub aura_network_url: String,
    /// Optional `aura-os-server` base URL.
    ///
    /// When set, `HttpDomainApi` routes spec / task / project / log
    /// writes through this base URL instead of hitting `aura-storage`
    /// directly. That matters because `aura-os-server` is the
    /// component that runs the side effects on those writes —
    /// mirroring spec markdown to
    /// `<workspace_root>/spec/<slug>.md` on disk, broadcasting the
    /// change to the project's SSE stream, and attaching JWT billing
    /// headers on the outbound storage call. The same URL is also what
    /// powers the cross-agent `send_to_agent` runtime hook
    /// (see `session::cross_agent_hook::AuraServerAgentHook`); when
    /// `None` the resolver wires no `agent_control_hook`, so every
    /// `send_to_agent` call falls through to `missing_runtime_hook`.
    ///
    /// Populated from `AURA_OS_SERVER_URL` (or the legacy
    /// `AURA_SERVER_BASE_URL`). [`NodeConfig::from_env`] additionally
    /// auto-fills this with `http://127.0.0.1:19847` when the env var
    /// is unset *and* `bind_addr` is loopback, which is the
    /// aura-os-desktop local-dev case (the desktop's embedded
    /// aura-os-server always binds `PREFERRED_PORT = 19847`). Swarm
    /// pods bind `0.0.0.0` and therefore keep the historical `None`
    /// fallback to `aura-storage` direct. [`Default::default`] stays
    /// `None` so hand-built configs / tests are unaffected.
    pub aura_os_server_url: Option<String>,
    /// Shared-secret bearer token required by every protected route.
    ///
    /// Only consulted when [`Self::require_auth`] is `true`. Populated
    /// by [`resolve_auth_token`] during `Node::run` from (in order)
    /// `AURA_NODE_AUTH_TOKEN`, a persisted `$data_dir/auth_token` file,
    /// or a freshly-minted 32-byte random hex value. Default is
    /// `"test"` strictly for test fixtures — production startup
    /// overwrites it before the router is built when auth is enabled,
    /// and clears it to the empty string when auth is disabled. Do
    /// **not** log or print this value anywhere; the router middleware
    /// reads it via constant-time compare and the `TraceLayer` is
    /// configured to omit the `Authorization` header.
    pub auth_token: String,
    /// Whether to attach the bearer-token auth middleware to the
    /// router and mint / require a token at startup.
    ///
    /// Default is `false` — the node accepts unauthenticated requests
    /// on its loopback-bound listener, which matches how most local
    /// development workflows run. Set `AURA_NODE_REQUIRE_AUTH=1`
    /// (or `true`) to re-enable the full shared-secret enforcement
    /// path: [`resolve_auth_token`] runs on startup, the router layers
    /// `require_bearer_mw` onto every protected route, and the
    /// `/stream/:run_id` WebSocket handler keeps its
    /// belt-and-suspenders check. Leaving this off on a non-loopback
    /// listener is a deliberate trust decision; pair it with firewall
    /// or network-level controls.
    pub require_auth: bool,
    /// Operator opt-in that permits effective FullAccess sessions to bypass
    /// command, binary, and shell-script allowlists.
    ///
    /// This is only the operator ceiling. Runtime session wiring still requires
    /// the agent/user permission state to be effectively FullAccess before any
    /// per-session bypass is enabled.
    pub allow_unrestricted_full_access: bool,
}

impl std::fmt::Debug for NodeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeConfig")
            .field("data_dir", &self.data_dir)
            .field("project_base", &self.project_base)
            .field("bind_addr", &self.bind_addr)
            .field("sync_writes", &self.sync_writes)
            .field("record_window_size", &self.record_window_size)
            .field("orbit_url", &self.orbit_url)
            .field("aura_storage_url", &self.aura_storage_url)
            .field("aura_network_url", &self.aura_network_url)
            .field("aura_os_server_url", &self.aura_os_server_url)
            .field("auth_token", &"***")
            .field("require_auth", &self.require_auth)
            .field(
                "allow_unrestricted_full_access",
                &self.allow_unrestricted_full_access,
            )
            .finish()
    }
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            project_base: None,
            bind_addr: "127.0.0.1:8080".to_string(),
            sync_writes: false,
            record_window_size: 50,
            orbit_url: "https://orbit-sfvu.onrender.com".to_string(),
            aura_storage_url: "https://aura-storage.onrender.com".to_string(),
            aura_network_url: "https://aura-network.onrender.com".to_string(),
            aura_os_server_url: None,
            auth_token: DEFAULT_TEST_AUTH_TOKEN.to_string(),
            require_auth: false,
            allow_unrestricted_full_access: false,
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir().map_or_else(
        || PathBuf::from("./aura_data"),
        |path| path.join("aura").join("node"),
    )
}

/// Classify a `host:port` bind string as loopback.
///
/// Used by [`NodeConfig::from_env`] to decide whether the
/// `aura-os-desktop` loopback default for `aura_os_server_url` should
/// fire. Conservative on purpose: only the canonical IPv4 / IPv6
/// loopback literals and the textual `localhost` count. `0.0.0.0` and
/// `::` (the wildcard binds Docker / swarm pods use) intentionally do
/// not match — those deployments need an explicit `AURA_OS_SERVER_URL`
/// pointing at a public URL.
fn bind_addr_is_loopback(addr: &str) -> bool {
    let trimmed = addr.trim();
    if let Ok(socket) = trimmed.parse::<std::net::SocketAddr>() {
        return socket.ip().is_loopback();
    }
    // Hostname:port form (e.g. `localhost:8080`) doesn't parse as
    // `SocketAddr` so handle it manually.
    if let Some((host, _)) = trimmed.rsplit_once(':') {
        return host.eq_ignore_ascii_case("localhost");
    }
    false
}

impl NodeConfig {
    /// Load configuration from environment variables.
    #[must_use]
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(val) = std::env::var("AURA_DATA_DIR").or_else(|_| std::env::var("DATA_DIR")) {
            config.data_dir = PathBuf::from(val);
        }
        if let Ok(val) = std::env::var("AURA_LISTEN_ADDR").or_else(|_| std::env::var("BIND_ADDR")) {
            config.bind_addr = val;
        }
        if let Ok(val) = std::env::var("SYNC_WRITES") {
            config.sync_writes = val == "true" || val == "1";
        }
        if let Ok(val) = std::env::var("RECORD_WINDOW_SIZE") {
            if let Ok(n) = val.parse() {
                config.record_window_size = n;
            }
        }
        if let Ok(val) = std::env::var("ORBIT_URL") {
            config.orbit_url = val;
        }
        if let Ok(val) = std::env::var("AURA_STORAGE_URL") {
            config.aura_storage_url = val;
        }
        if let Ok(val) = std::env::var("AURA_NETWORK_URL") {
            config.aura_network_url = val;
        }
        if let Ok(val) =
            std::env::var("AURA_OS_SERVER_URL").or_else(|_| std::env::var("AURA_SERVER_BASE_URL"))
        {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                config.aura_os_server_url = Some(trimmed.to_string());
            }
        }
        // Local-desktop convenience default: aura-os-desktop's embedded
        // aura-os-server always binds `PREFERRED_PORT = 19847` on
        // loopback, so when the harness is also bound to loopback (the
        // sidecar case) we can wire the `send_to_agent` runtime hook
        // and the HttpDomainApi spec/task overrides without making the
        // operator set `AURA_OS_SERVER_URL` by hand. Skipped for
        // non-loopback binds (Docker / swarm pods on 0.0.0.0) so the
        // historical "post directly to aura-storage" fallback in
        // `HttpDomainApi::specs_tasks_base_url` stays intact for
        // remote deployments — those operators continue to set
        // `AURA_OS_SERVER_URL` explicitly to a public URL, and any
        // explicit value still wins because this only runs when
        // `is_none()`.
        if config.aura_os_server_url.is_none() && bind_addr_is_loopback(&config.bind_addr) {
            config.aura_os_server_url = Some(DESKTOP_LOOPBACK_OS_SERVER_URL.to_string());
        }
        if let Ok(val) = std::env::var("AURA_PROJECT_BASE") {
            if !val.is_empty() {
                config.project_base = Some(PathBuf::from(val));
            }
        }
        if let Ok(val) = std::env::var("AURA_NODE_AUTH_TOKEN") {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                config.auth_token = trimmed.to_string();
            }
        }
        if let Ok(val) = std::env::var("AURA_NODE_REQUIRE_AUTH") {
            let v = val.trim();
            config.require_auth = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Ok(val) = std::env::var("AURA_ALLOW_UNRESTRICTED_FULL_ACCESS") {
            let v = val.trim();
            config.allow_unrestricted_full_access = v == "1" || v.eq_ignore_ascii_case("true");
        }
        config
    }

    /// Get the `RocksDB` path.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        aura_agent::session_bootstrap::resolve_store_path(&self.data_dir)
    }

    /// Get the workspaces base path.
    #[must_use]
    pub fn workspaces_path(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    /// Remap an incoming project path through `project_base` when configured.
    ///
    /// Extracts the last path component (the project slug) and returns
    /// `{project_base}/{slug}`. When `project_base` is `None` the path passes
    /// through unchanged.
    #[must_use]
    pub fn resolve_project_path(&self, incoming: &std::path::Path) -> PathBuf {
        if let Some(ref base) = self.project_base {
            let slug = incoming
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("default");
            base.join(slug)
        } else {
            incoming.to_path_buf()
        }
    }

    /// Resolve the canonical workspace directory for a project by name.
    ///
    /// This is the single source of truth for where a project's files live.
    /// - Remote VMs (`project_base` set): `{project_base}/{slug}` e.g. `/home/aura/testaaa`
    /// - Local dev (`project_base` unset): `{data_dir}/workspaces/{slug}`
    #[must_use]
    pub fn resolve_workspace_for_project(&self, project_name: &str) -> PathBuf {
        let slug = slugify(project_name);
        if let Some(ref base) = self.project_base {
            base.join(&slug)
        } else {
            self.workspaces_path().join(&slug)
        }
    }

    /// Check whether a path is allowed for file operations.
    ///
    /// Thin wrapper around [`Self::resolve_allowed_path`] retained for
    /// callers that only care whether a path is legal and don't need the
    /// canonical form. New code should prefer `resolve_allowed_path` so
    /// traversal attempts can be distinguished from missing files.
    #[must_use]
    pub fn is_allowed_path(&self, path: &Path) -> bool {
        self.resolve_allowed_path(path).is_ok()
    }

    /// Resolve `input` to a canonical path inside the workspace root.
    ///
    /// Replaces the previous `Path::starts_with` check against the raw
    /// input, which was bypassable with `../` sequences that only
    /// normalised after canonicalisation. The new implementation:
    ///
    /// 1. Canonicalises the workspace root (so symlinks / junctions
    ///    anywhere in the root's ancestry resolve to their real target).
    /// 2. Joins relative `input`s onto the root before canonicalising.
    /// 3. Canonicalises the candidate path, which follows symlinks to
    ///    their real target.
    /// 4. Verifies the canonical candidate lives under the canonical
    ///    root via `starts_with`. Any traversal, symlink, or junction
    ///    that lands outside fails here.
    ///
    /// Relative paths, absolute paths, `.`, and empty inputs are all
    /// accepted — empty / `.` inputs resolve to the root itself.
    ///
    /// # Errors
    ///
    /// * [`PathError::NotFound`] — the candidate path does not exist.
    /// * [`PathError::Escapes`] — the candidate's canonical form is not
    ///   a descendant of the canonical workspace root.
    /// * [`PathError::NotPermitted`] — the workspace root itself
    ///   cannot be canonicalised (missing / permission denied), or the
    ///   candidate's canonicalisation failed for a non-NotFound reason.
    pub fn resolve_allowed_path(&self, input: &Path) -> Result<PathBuf, PathError> {
        let root = self.file_root();
        let canonical_root = std::fs::canonicalize(&root).map_err(|e| {
            PathError::NotPermitted(format!(
                "workspace root unavailable ({}): {e}",
                root.display()
            ))
        })?;
        let canonical_root = strip_unc_prefix(&canonical_root);

        let candidate = if input.as_os_str().is_empty() || input == Path::new(".") {
            root.clone()
        } else if input.is_absolute() {
            input.to_path_buf()
        } else {
            root.join(input)
        };

        let canonical_candidate = match std::fs::canonicalize(&candidate) {
            Ok(p) => strip_unc_prefix(&p),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(PathError::NotFound(candidate));
            }
            Err(e) => {
                return Err(PathError::NotPermitted(format!(
                    "canonicalize({}): {e}",
                    candidate.display()
                )));
            }
        };

        if !canonical_candidate.starts_with(&canonical_root) {
            return Err(PathError::Escapes(canonical_candidate));
        }

        Ok(canonical_candidate)
    }

    /// Return the root directory for file browsing (project_base or workspaces).
    #[must_use]
    pub fn file_root(&self) -> PathBuf {
        if let Some(ref base) = self.project_base {
            base.clone()
        } else {
            self.workspaces_path()
        }
    }
}

/// Strip the `\\?\` verbatim prefix that Windows `canonicalize()` adds.
/// On non-Windows this is a no-op.
fn strip_unc_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\")
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// File name used for persisted per-install auth tokens under `$data_dir`.
const AUTH_TOKEN_FILENAME: &str = "auth_token";

/// Resolve the bearer secret the node will enforce on protected routes.
///
/// Source order, matching the Wave 5 / phase-4 security hardening spec:
///
/// 1. `AURA_NODE_AUTH_TOKEN` environment variable — if present and
///    non-empty, used verbatim. Not persisted and not printed; the
///    operator is assumed to have deliberately set it.
/// 2. `$data_dir/auth_token` file — if present and non-empty, reused
///    so operators don't get a fresh token on every restart. On Unix
///    the file's mode must be `0600`; anything more permissive is
///    treated as tampered and a fresh token is minted over the top.
/// 3. Otherwise a new 32-byte random value is minted (hex-encoded to
///    64 chars), written to `$data_dir/auth_token` with mode `0600`
///    on Unix, and printed **once** to stderr so the launching shell
///    can copy it into client tooling.
///
/// Errors bubble up as `io::Error`; the caller (`Node::run`) decides
/// whether to abort startup or proceed with a best-effort fallback.
pub fn resolve_auth_token(data_dir: &Path) -> std::io::Result<String> {
    if let Ok(env_val) = std::env::var("AURA_NODE_AUTH_TOKEN") {
        let trimmed = env_val.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let token_path = data_dir.join(AUTH_TOKEN_FILENAME);
    if let Some(existing) = read_existing_auth_token(&token_path) {
        return Ok(existing);
    }

    let token = mint_auth_token();
    std::fs::create_dir_all(data_dir)?;
    write_auth_token(&token_path, &token)?;
    // Log-once to stderr matches the `jupyter notebook` UX: a human
    // can copy the token into curl/browser tooling on first launch.
    // Do NOT promote this to stdout, the tracing logger, or any file
    // other than `$data_dir/auth_token` — the token is only as strong
    // as its handling.
    eprintln!(
        "aura-runtime auth token: {token} (store in client: export AURA_NODE_AUTH_TOKEN={token})"
    );
    Ok(token)
}

/// Try to reuse an on-disk auth token.
///
/// Returns `None` when the file is missing, empty, unreadable, or —
/// on Unix — has permissions more permissive than `0600`. In each of
/// those cases the caller mints a fresh token instead so an attacker
/// who managed to drop a world-readable file can't pin the secret.
fn read_existing_auth_token(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).ok()?;
        // Reject any group / world bits — `0o077` tests "other than
        // owner-rw"; anything matching is considered tampered.
        if meta.permissions().mode() & 0o077 != 0 {
            return None;
        }
    }
    Some(trimmed.to_string())
}

/// Mint a 32-byte random hex token (~256 bits of entropy).
///
/// Uses two `uuid::Uuid::new_v4()` values concatenated so we don't have
/// to pull in `rand` just for this — `uuid` is already a workspace
/// dependency and v4 UUIDs are cryptographically random on every
/// supported platform. Mirrors the pattern used by the terminal-mode
/// `api_server` minted in phase 3.
fn mint_auth_token() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

/// Persist `token` at `path` with tight permissions.
///
/// On Unix the file is created with mode `0600` via `OpenOptions::mode`
/// so we never briefly expose a world-readable file. On Windows we fall
/// back to a plain write; NTFS ACLs are inherited from the parent data
/// directory and the token file never contains anything a Windows user
/// wouldn't already have access to (everything runs under their own
/// account).
fn write_auth_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
    }
    Ok(())
}

fn slugify(name: &str) -> String {
    let s = name
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-")
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "");
    if s.is_empty() {
        "unnamed-project".to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests;
