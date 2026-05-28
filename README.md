<p align="center">
  <strong style="font-size: 2em;">AURA</strong>
</p>

---

<p align="center">
  <strong>Deterministic Multi-Agent Runtime</strong><br/>
  An append-only, pluggable-reasoning runtime for running many agents concurrently with sandboxed tool execution.
</p>

<p align="center">
  <a href="#overview">Overview</a> &nbsp;·&nbsp;
  <a href="#quick-start">Quick Start</a> &nbsp;·&nbsp;
  <a href="#binaries">Binaries</a> &nbsp;·&nbsp;
  <a href="#cli-reference">CLI</a> &nbsp;·&nbsp;
  <a href="#architecture">Architecture</a> &nbsp;·&nbsp;
  <a href="#http--websocket-api">API</a> &nbsp;·&nbsp;
  <a href="#configuration">Configuration</a> &nbsp;·&nbsp;
  <a href="#development">Development</a>
</p>

## Overview

Aura is a deterministic multi-agent runtime for running many agents concurrently. Every agent maintains an append-only record log, a deterministic kernel advances state by consuming transactions, and reasoning is delegated to a proxy-routed LLM provider. All side effects flow through authorized executors so the full history is replayable from the record alone.

The runtime supports interactive terminal sessions (TUI), headless server deployments, and long-running automaton workflows — all backed by the same kernel, storage, and reasoning stack.

> This repository (`aura-harness`) is the Cargo workspace that builds the Aura runtime (`aura`, `aura-node`). It is distinct from the sibling `aura-swarm` repository, which is a Firecracker/Kubernetes platform for hosting Aura agents.

Core ideas:

1. **The Record.** The fundamental unit of truth. Every agent has an append-only log of record entries, strictly ordered by sequence number. All state is derivable from the record; there is no hidden state.
2. **The Kernel.** A deterministic processor that builds context from the record, calls the reasoner, enforces policy, executes actions through the executor, and commits new entries. Given the same record, the kernel always produces the same output.
3. **Reasoning.** Probabilistic LLM calls are isolated behind a provider trait. Production traffic routes through the JWT-authenticated `aura-router` proxy; a mock provider is available for tests.
4. **Tools & Executors.** All side effects (filesystem, shell commands, domain APIs, automaton actions) are explicit. The executor router dispatches authorized actions and captures structured effects, keeping the kernel boundary clean.
5. **Memory & Skills.** Per-agent memory (facts, events, procedures) and `SKILL.md`-based skill packages extend an agent's abilities at runtime without widening the deterministic kernel.

## Principles

1. **Per-Agent Order** — Record entries are strictly ordered by sequence number; no reordering, no gaps.
2. **Atomic Commit** — Transaction processing is all-or-nothing via RocksDB batch writes.
3. **No Hidden State** — All state is replayable from the record. If it is not in the log, it did not happen.
4. **Deterministic Kernel** — The kernel advances only by consuming transactions. Same input, same output.
5. **Explicit Side Effects** — Every tool call flows through an authorized executor; effects are captured and recorded.
6. **Open Source** — MIT-licensed Rust workspace. Every layer is auditable and reusable.

## Prerequisites

`aura-harness` is a self-contained Cargo workspace; every crate it depends on (including the WebSocket `aura-protocol` types) lives under [`crates/`](crates). There is no sibling-repo checkout step.

RocksDB builds require LLVM/Clang. On Linux, the `keyring` crate's Secret Service backend links against `libdbus-1`, so the workspace also needs `libdbus-1-dev` and `pkg-config` at build time (e.g. `sudo apt install libdbus-1-dev pkg-config` on Debian/Ubuntu, `sudo dnf install dbus-devel pkgconf-pkg-config` on Fedora). macOS and Windows builds need no extra system packages for keyring.

## Quick Start

```sh
# Build the full workspace (release)
cargo build --release

# Run the TUI (proxy mode — no API key needed)
cargo run

# Run the same binary headless
cargo run -- run --ui none

# Run the standalone node server (binary name stays `aura-node`)
cargo run -p aura-runtime --bin aura-node
```

### Docker

The Dockerfile builds the workspace from the repo root — no external sibling checkout is required:

```sh
# in aura-harness/
docker build -t aura .
docker run -p 8080:8080 aura
```

The image runs `aura run --ui none` as a non-root user, exposes `:8080`, and defaults `AURA_DATA_DIR=/data`. See [`Dockerfile`](Dockerfile) for the full recipe.

## Binaries

This workspace ships two binaries:

| Binary | Entry point | Purpose |
|--------|-------------|---------|
| `aura` | [`src/main.rs`](src/main.rs) | **Canonical CLI entry point.** Thin root binary that delegates to `aura_surface_cli::run`; TUI by default; headless node with `run --ui none`; also hosts `login` / `logout` / `whoami` / `hello`. |
| `aura-node` | [`crates/aura-runtime/src/main.rs`](crates/aura-runtime/src/main.rs) | Standalone headless server (HTTP + WebSocket API). Binary name `aura-node` shipped from the `aura-runtime` crate (the HTTP/WS gateway + composition root). Orchestration lives in [`crates/aura-engine/`](crates/aura-engine), HTTP `DomainApi` in [`crates/aura-domain-http/`](crates/aura-domain-http), and the concrete subagent dispatcher in [`crates/aura-fleet-subagent/`](crates/aura-fleet-subagent). |

> **Historical:** earlier design drafts and the v0.1.0 specs referred
> to a separate `aura-cli` crate. That crate was retired in Wave 4 of
> the refactor (2026) and never shipped in this workspace. Its intended
> surface — interactive REPL, approvals, slash commands, record
> tailing — is now split between the `aura` binary at the workspace
> root (`src/`) for the interactive TUI and `aura-node` for the
> headless server. Anyone reading a spec that mentions `aura-cli/src/...`
> should map those paths onto `src/...` in the root crate.

## CLI Reference

Defined in [`crates/aura-surface-cli/src/cli.rs`](crates/aura-surface-cli/src/cli.rs):

| Command | Description |
|---------|-------------|
| `aura run` (default) | Run the agent. Flags below. |
| `aura login` | Authenticate with zOS and store a JWT for proxy mode. |
| `aura logout` | Clear stored credentials. |
| `aura whoami` | Show current authentication status. |
| `aura hello` | Print `Hello, World!` and exit (Spec 01 smoke test). |
| `aura migrate [--dry-run]` | Migrate aura state (Phase 4a stub today). |
| `aura plugins <install\|list\|enable\|disable>` | Manage declarative plugins under `AURA_HOME/plugins/` (Phase 4b). |
| `aura agents <inspect\|reap>` | Inspect or reap live + orphaned subagents (Phase 7b). |

Global flags (apply to every subcommand):

| Flag | Default | Description |
|------|---------|-------------|
| `--mode <agent\|plan\|ask\|debug>` | (see resolution chain) | Top rung of the documented `AgentMode` resolution priority (CLI > TUI slash > SDK field > daemon default > `Agent` fallback). Resolved at session start by `aura_fleet_daemon::resolve_session_mode`. |

Flags for `aura run`:

| Flag | Default | Description |
|------|---------|-------------|
| `--ui <terminal\|none>` | `terminal` | Terminal UI (ratatui) or headless node. |
| `--theme <name>` | `cyber` | One of `cyber`, `matrix`, `synthwave`, `minimal`. |
| `-d, --dir <path>` | -- | Override working / data directory. |
| `--provider <anthropic\|mock>` | `anthropic` | Model provider for the current session. |
| `-v, --verbose` | off | Enable verbose tracing output. |
| `--allow-unrestricted-full-access` | off | Permit FullAccess sessions to bypass command allowlists (operator opt-in). |

## Architecture

The workspace is organized into **ten layers** with strict downward-only dependencies (enforced by [`tests/layer_boundary.rs`](tests/layer_boundary.rs)):

```text
core  <  store  <  config  <  model  <  context  <
plugin  <  exec  <  agent   <  fleet  <  surface
```

| Layer    | Purpose                                                                          | Representative crates                                                                          |
|----------|----------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| core     | Behavior-free IDs, capability enums, mode primitives, wire types.                | `aura-core-types`, `aura-core-modes`, `aura-core-permissions`, `aura-core-auth`, `aura-core-protocol`, `aura-core`, `aura-protocol`. |
| store    | Durable storage; record-log append surface is sealed to the kernel.              | `aura-store-db`, `aura-store-record`, `aura-store-snapshot`, `aura-store`. |
| config   | Single source of truth for env vars + TOML config.                               | `aura-config`. |
| model    | Provider-agnostic LLM trait + streaming completions (proxy-routed only).         | `aura-model-reasoner`, `aura-reasoner`. |
| context  | Read-only context assembly: prompts, memory, compaction, skills.                 | `aura-context-prompts`, `aura-context-memory`, `aura-context-compaction`, `aura-context-skills` (+ legacy shells). |
| plugin   | Plugin manifest schema, in-process API, hooks, MCP, connectors.                  | `aura-plugin-api`, `aura-plugin-core`, `aura-plugin-hooks`, `aura-plugin-mcp`, `aura-plugin-connectors`. |
| exec     | Tool catalog, runner, sandbox, policy, isolation, conflict locks.                | `aura-exec-conflict`, `aura-exec-isolation`, `aura-exec-policy`, `aura-exec-sandbox`, `aura-exec-tools`, `aura-exec-runner`, `aura-tools`. |
| agent    | Deterministic kernel + AgentLoop + steering + subagent derivation.               | `aura-agent-kernel`, `aura-agent-loop`, `aura-agent-steering`, `aura-agent-subagent`, `aura-agent`, `aura-kernel`. |
| fleet    | Multi-agent registry, spawn, dispatch, quota, mailbox, daemon, subagent dispatcher. | `aura-fleet-registry`, `aura-fleet-spawn`, `aura-fleet-dispatch`, `aura-fleet-quota`, `aura-fleet-mailbox`, `aura-fleet-daemon`, `aura-fleet-subagent`. |
| surface  | Composition roots: CLI, TUI, SDK, automaton, auth, HTTP/WS gateway, orchestration engine, domain HTTP client. | `aura-surface-cli`, `aura-surface-sdk`, `aura-surface-terminal`, `aura-surface-automaton`, `aura-surface-auth`, `aura-runtime`, `aura-engine`, `aura-domain-http`, `aura-terminal`, `aura-automaton`, `aura-auth`. |

Full per-crate reference (purpose, key types, modules) lives in [`docs/architecture.md`](docs/architecture.md). All members are declared in [`Cargo.toml`](Cargo.toml) under `[workspace].members`.

### Project structure

```
aura-harness/
  Cargo.toml                # workspace root + `aura` binary
  Dockerfile                # multi-stage build, headless on :8080
  .env.example              # environment variable template
  index.html                # landing page
  src/                      # `aura` binary entry (CLI body lives in aura-surface-cli)
  crates/                   # 58 crates organized into the 10 layers above
                            #   (see docs/architecture.md for the per-crate reference)
  tests/                    # workspace integration, e2e, proptest, pipeline tests
  docs/                     # supplementary documentation
    architecture.md         #   full layered crate reference + user flows
    invariants.md           #   the architectural invariants + enforcement map
  docker/                   # docker build assets
  scripts/                  # check_invariants.sh + helpers
  .github/                  # CI workflows (ci.yml, security.yml, invariants.yml)
```

### System diagram

The kernel boundary cuts at the `agent` layer; everything below it is downward-only by layer rule.

```
                             ┌──────────────────────────────────┐
                             │           Entry Points           │
                             │  aura (TUI)  ·  aura --ui none  │
                             │  aura-node                       │
                             └──────────────┬───────────────────┘
                                            │
                             ┌──────────────▼───────────────────┐
                             │         HTTP / WebSocket         │
                             │      Router (Axum on :8080)      │
                             │  (routes listed below)           │
                             └──────────────┬───────────────────┘
                                            │
                    ┌───────────────────────▼──────────────────────────┐
                    │                  Scheduler                       │
                    │   per-agent tokio::Mutex  ·  DashMap registry   │
                    └───┬──────────────┬──────────────┬───────────────┘
                        │              │              │
                   ┌────▼────┐   ┌─────▼────┐   ┌────▼────┐
                   │ Worker  │   │  Worker  │   │ Worker  │  (one per agent)
                   │ Dequeue │   │ Dequeue  │   │ Dequeue │
                   │ Process │   │ Process  │   │ Process │
                   │ Commit  │   │ Commit   │   │ Commit  │
                   └────┬────┘   └────┬─────┘   └────┬────┘
                        └─────────────┼──────────────┘
                                      │
                    ┌─────────────────▼───────────────────────────────┐
                    │              Kernel (Deterministic)              │
                    │  Build context  ·  Call Reasoner  ·  Policy     │
                    │  Execute actions  ·  Build RecordEntry          │
                    └─────┬──────────────────┬──────────────┬────────┘
                          │                  │              │
             ┌────────────▼─────┐  ┌─────────▼────┐  ┌─────▼──────────┐
             │     Reasoner     │  │   Executor   │  │     Store      │
             │                  │  │   (Tools)    │  │   (RocksDB)    │
             │  proxy ──► Router│  │  FS · Cmd    │  │  record        │
             │  direct ► Claude │  │  Domain      │  │  agent_meta    │
             └──────┬───────────┘  │  Automaton   │  │  inbox         │
                    │              └──────────────┘  │  memory_*      │
                    │                                │  agent_skills  │
                    │                                └────────────────┘
      ┌─────────────┼──────────────────────────────┐
      │             │                              │
 ┌────▼──────┐ ┌────▼──────────┐  ┌───────────────▼───────────────┐
 │ Aura      │ │  Anthropic   │  │     Domain Services           │
 │ Router    │ │  API         │  │  Orbit · Aura Storage         │
 │ (proxy)   │ │  (direct)    │  │  Aura Network                 │
 └───────────┘ └──────────────┘  └───────────────────────────────┘
```

## HTTP / WebSocket API

All routes are defined in `crates/aura-runtime/src/gateway/middleware.rs` (`create_router`), grouped under `crates/aura-runtime/src/gateway/handlers/`; the shared `RouterState` lives in `crates/aura-runtime/src/gateway/state.rs`. Names use Axum path-parameter syntax.

### Health & workspace

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/health` | Liveness probe. |
| GET | `/api/files` | List files in the configured workspace root. |
| GET | `/api/read-file` | Read a file from the workspace root. |
| GET | `/workspace/resolve` | Resolve a project/workspace slug to a filesystem path. |

### Transactions & records

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/tx` | Submit a transaction for an agent. |
| GET  | `/tx/status/:agent_id/:tx_id` | Status of a submitted transaction. |
| GET  | `/agents/:agent_id/head` | Latest record sequence for an agent. |
| GET  | `/agents/:agent_id/record` | Paginated record scan. |

### Runs (chat / dev-loop / task-run)

A "run" is the canonical entry point — any chat session, dev-loop automaton, or single-task automaton is started by POSTing an [`aura_protocol::RuntimeRequest`](crates/aura-protocol/src/runtime_request.rs) to `/v1/run`. The [`RuntimeRunResponse`](crates/aura-protocol/src/runtime_request.rs) carries a `run_id` plus the WS path the client should open to receive events (and, on chat runs, to send user messages).

| Method | Path | Purpose |
|--------|------|---------|
| POST | `/v1/run` | Start a run (chat / dev-loop / task-run). Body is `RuntimeRequest`; returns `RuntimeRunResponse { run_id, event_stream_url }`. |
| GET  | `/v1/run/list` | List active runs. |
| GET  | `/v1/run/:run_id/status` | Status for one run. |
| POST | `/v1/run/:run_id/pause` | Pause a run. |
| POST | `/v1/run/:run_id/stop` | Stop a run. |

### Memory

Canonical routes are mounted under `/memory/...`; compatibility aliases are mounted under `/api/agents/:agent_id/memory/...`. Both surfaces cover:

- Facts: list / create / update / delete, fetch by key.
- Events: list / create / delete, bulk-delete.
- Procedures: list / create / update / delete.
- `GET /memory/:agent_id/snapshot` — full memory snapshot.
- `POST /memory/:agent_id/wipe` — clear all memory for an agent.
- `GET /memory/:agent_id/stats` — counts, token budgets.
- `POST /memory/:agent_id/consolidate` — trigger consolidation.

### Skills

| Method | Path | Purpose |
|--------|------|---------|
| GET, POST | `/api/skills` | List available skills / install a skill. |
| GET | `/api/skills/:name` | Skill details. |
| POST | `/api/skills/:name/activate` | Activate a skill. |
| GET, POST | `/api/agents/:agent_id/skills` | Per-agent install list / install. |
| DELETE | `/api/agents/:agent_id/skills/:name` | Uninstall a skill from an agent. |

Legacy harness aliases for skill list/install/uninstall are mounted under `/api/harness/agents/:agent_id/skills...` for backward compatibility.

### WebSocket

| Path | Purpose |
|------|---------|
| `/ws/terminal` | Terminal bridge used by the TUI. |
| `/stream/:run_id` | Per-run event stream. Bidirectional on `Chat` runs (user messages in, deltas / tool calls / approvals out); event-only on `DevLoop` / `TaskRun` runs. The `run_id` is the value returned synchronously by `POST /v1/run`. |

## Memory

`aura-memory` adds per-agent long-term memory backed by RocksDB column families:

- **Facts** — durable key/value claims (`MEMORY_FACTS`).
- **Events** — episodic events with time index (`MEMORY_EVENTS`, `MEMORY_EVENT_INDEX`).
- **Procedures** — repeated step sequences detected over time (`MEMORY_PROCEDURES`).

Writes flow through a two-stage pipeline (heuristic extractor + optional LLM refiner, see [`crates/aura-memory/src/write_pipeline.rs`](crates/aura-memory/src/write_pipeline.rs) and [`crates/aura-memory/src/refinement.rs`](crates/aura-memory/src/refinement.rs)). `MemoryRetriever` injects a size-budgeted slice of memory into the kernel context on each turn.

## Skills

`aura-skills` loads `SKILL.md` packages from (in precedence order):

1. Workspace — `{workspace}/skills/`
2. Agent-personal — `~/.aura/agents/{id}/skills/`
3. Personal — `~/.aura/skills/`
4. Extra directories from config
5. Bundled skills shipped with the runtime

`SkillManager` exposes activation and prompt injection; `SkillInstallStore` persists per-agent installs in the `AGENT_SKILLS` column family. See [`crates/aura-skills/src/lib.rs`](crates/aura-skills/src/lib.rs).

## Configuration

The node reads configuration from environment variables via `NodeConfig::from_env()` in [`crates/aura-runtime/src/config/mod.rs`](crates/aura-runtime/src/config/mod.rs). Copy [`.env.example`](.env.example) as a starting point.

### LLM routing

All LLM traffic flows through the AURA router (proxy) using a per-request JWT. There is no direct-provider path: `aura-harness` does not call Anthropic (or any other provider) on its own.

| Variable | Default | Description |
|----------|---------|-------------|
| `AURA_ROUTER_URL` | `https://aura-router.onrender.com` | Proxy router endpoint. |
| `AURA_ROUTER_JWT` | — | JWT for terminal/CLI sessions. WebSocket clients supply their own. |
| `AURA_DEFAULT_MODEL` | `claude-opus-4-6` (`aura_reasoner::ENV_FALLBACK_MODEL`) | Model identifier sent to the router **only** when the request did not pin a model itself; sessions, dev-loop runs, and task runs all carry an explicit model end-to-end. (Legacy `AURA_ANTHROPIC_MODEL` is still read as a fallback for one release.) |
| `AURA_DEFAULT_FALLBACK_MODEL` | — | Optional secondary model used on 429/529 retries. |
| `AURA_MODEL_TIMEOUT_MS` | `60000` | LLM request timeout. |
| `AURA_LLM_MAX_RETRIES` | `8` | Per-model retry budget before falling back. |
| `AURA_DISABLE_PROMPT_CACHING` | — | Set to `1`/`true` to disable Anthropic prompt-caching directives. |

### Node runtime

| Variable | Default | Description |
|----------|---------|-------------|
| `AURA_DATA_DIR` (alias `DATA_DIR`) | OS local app data `aura/node`; `./aura_data` fallback | Data directory for RocksDB and workspaces. Set explicitly to share state or keep repo-local data. |
| `AURA_LISTEN_ADDR` (alias `BIND_ADDR`) | `127.0.0.1:8080` | HTTP server bind address. |
| `SYNC_WRITES` | `false` | Enable sync writes (`true`/`1` to enable) to RocksDB. |
| `RECORD_WINDOW_SIZE` | `50` | Kernel context record window. |
| `AURA_PROJECT_BASE` | — | Remaps incoming project paths to `{base}/{slug}` (remote VM mode). |
| `ORBIT_URL` | `https://orbit-sfvu.onrender.com` | Orbit service URL. |
| `AURA_STORAGE_URL` | `https://aura-storage.onrender.com` | Aura Storage service URL. |
| `AURA_NETWORK_URL` | `https://aura-network.onrender.com` | Aura Network service URL. |
| `AURA_NODE_REQUIRE_AUTH` | `false` | Opt-in bearer-token gate. When off, the gateway does not attach `require_bearer_mw`, the `/stream/:run_id` WebSocket skips its inline check, and the embedded TUI API server mounts its routes without auth. Set `1` / `true` to re-enable shared-secret enforcement. |
| `AURA_NODE_AUTH_TOKEN` | — | Shared-secret bearer token consumed when `AURA_NODE_REQUIRE_AUTH=1`. When unset, the node reads (or mints) `$AURA_DATA_DIR/auth_token` and prints it to stderr on first launch. Ignored when auth is disabled. |

### Authentication

By default (`AURA_NODE_REQUIRE_AUTH` unset or `0`), aura-node accepts
requests without an `Authorization` header on its loopback-bound
listener. This matches most local development workflows and removes the
"copy the token out of stderr" step for first-run operators.

To restore the Wave 5 / phase-4 hardening posture — a shared-secret
bearer token enforced on every non-`/health` route, with a `401` for
missing or wrong tokens — set `AURA_NODE_REQUIRE_AUTH=1`. The node
will:

1. Resolve a token via `AURA_NODE_AUTH_TOKEN`, then
   `$AURA_DATA_DIR/auth_token` (mode `0600` on Unix), then a freshly
   minted 32-byte hex value printed to stderr on first run.
2. Attach `require_bearer_mw` to the protected sub-router.
3. Keep the belt-and-suspenders check in `/stream/:run_id`.
4. Print the embedded `aura` TUI API server token to stderr so a
   browser or curl can copy it.

Running a non-loopback listener (`AURA_LISTEN_ADDR=0.0.0.0:...`)
without auth is a deliberate trust decision; pair it with firewall or
network-level controls if you intend to leave auth off.

## Development

```bash
# Format
cargo fmt --all

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Test everything
cargo test --all --all-features

# Fast smoke test: node config
cargo test -p aura-runtime config::

# Check non-RocksDB crates (no LLVM required)
cargo check -p aura-core -p aura-kernel -p aura-reasoner
```

Further reading:

- [`docs/architecture.md`](docs/architecture.md) — full architecture reference.
- [`docs/invariants.md`](docs/invariants.md) — architectural invariants + enforcement map.
- [`CHANGELOG.md`](CHANGELOG.md) — per-phase refactor log.

## License

MIT
