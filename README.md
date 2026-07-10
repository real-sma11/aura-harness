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

Aura is a deterministic multi-agent runtime for running many agents concurrently. The system is built around **per-agent kernels with cross-agent parallelism**: each agent owns its own `Kernel` and append-only record log, a deterministic kernel advances that agent's state by consuming transactions, and reasoning is delegated to a proxy-routed LLM provider. All side effects flow through authorized executors, so any one agent's full history is replayable from its record alone — regardless of which other agents were running at the same time.

The runtime supports interactive terminal sessions (TUI), headless server deployments, and long-running automaton workflows — all backed by the same kernel, storage, and reasoning stack.

> This repository (`aura-harness`) is the Cargo workspace that builds the Aura runtime (`aura`, `aura-node`). It is distinct from the sibling `aura-swarm` repository, which is a Firecracker/Kubernetes platform for hosting Aura agents.

Core ideas:

1. **The Record.** The fundamental unit of truth. Every agent has its own append-only log of record entries, strictly ordered by per-agent sequence number. All state is derivable from the record; there is no hidden state.
2. **The Kernel.** A deterministic per-agent processor that builds context from the record, calls the reasoner, enforces policy, executes actions through the executor, and commits new entries. Given the same record, the kernel always produces the same output. The record-append surface is sealed to the kernel: only the kernel's `Arc<dyn WriteStore>` can commit a record entry.
3. **Modes & Policy.** Before any external effect, the resolved `AgentMode` (`Agent` / `Plan` / `Ask` / `Debug`) gates the action; the policy layer then narrows further per-tool. The mode gate runs *before* the policy check, not as a substitute for it.
4. **Reasoning.** Probabilistic LLM calls are isolated behind a provider trait and recorded by the kernel. There is exactly one real provider: an Anthropic-shaped client that always routes through the JWT-authenticated `aura-router` proxy. A mock provider is available for tests. There is no direct-to-Anthropic path.
5. **Tools & Executors.** All side effects (filesystem, shell commands, domain APIs, automaton actions) are explicit. The executor router dispatches authorized actions and captures structured effects, keeping the kernel boundary clean.
6. **Memory & Skills.** Per-agent memory (facts, events, procedures) and `SKILL.md`-based skill packages extend an agent's abilities at runtime without widening the deterministic kernel.

## Principles

1. **Per-Agent Order** — Within each agent, record entries are strictly ordered by sequence number; no reordering, no gaps.
2. **Atomic Commit** — Transaction processing is all-or-nothing via RocksDB batch writes (inbox dequeue + record append in one `WriteBatch`).
3. **No Hidden State** — All state is replayable from the record. If it is not in the log, it did not happen.
4. **Deterministic Kernel** — The kernel advances only by consuming transactions. Same input, same output; the per-agent context hash depends only on that agent's transaction and record window.
5. **Explicit Side Effects** — Every tool call flows through an authorized executor; effects are captured and recorded.
6. **Single Writer, Parallel Agents** — At most one task processes a given agent's queue at a time, while unrelated agents run fully concurrently. Determinism survives parallelism because every guarantee is per-agent.
7. **Open Source** — MIT-licensed Rust workspace. Every layer is auditable and reusable.

## Architectural Invariants

The runtime upholds 15 architectural invariants, grouped into five parts. Each is guarded by CI — either the ripgrep-band gate in [`scripts/check_invariants.sh`](scripts/check_invariants.sh) or a dedicated Rust test suite. Full definitions and the per-invariant enforcement map live in [`docs/invariants.md`](docs/invariants.md).

| Part | Theme | Invariants |
|------|-------|------------|
| A | **Kernel boundary & mediation** — the per-agent kernel is the sole external gateway; every state change and every LLM call passes through it and is recorded; gateways are transparent; the agent loop is isolated from kernel-owned resources. | §1, §2, §3, §8, §9 |
| B | **Policy & authorization** — every tool call passes the full `Policy::check()`; live `ask` decisions are session-scoped. | §4, §11 |
| C | **Record, audit, determinism & replay** — complete audit trail per entry; per-agent deterministic context hash; per-agent monotonic sequencing; per-agent append-only (sealed `WriteStore`). | §5, §6, §7, §10 |
| D | **Concurrency & cross-agent parallelism** — single writer per agent (store-backed claim) and per-parent spawn-audit lease; unrelated agents run in parallel. | §12, §15 |
| E | **Workspace & plugin structure** — strict downward-only layer stack; plugins run sandboxed and cannot bypass the kernel boundary. | §13, §14 |

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

For Render image deployments, use [`Dockerfile.render`](Dockerfile.render).
It binds to `0.0.0.0:10000`, exposes `/health`, and is published by the
`Publish GHCR image` workflow as `ghcr.io/cypher-asi/aura-harness:<tag>`.
Hosted, non-loopback services should require bearer auth:

```sh
AURA_LISTEN_ADDR=0.0.0.0:10000
AURA_NODE_REQUIRE_AUTH=1
AURA_NODE_AUTH_TOKEN=<shared-secret>
AURA_OS_SERVER_URL=https://YOUR-AURA-OS-SERVICE.onrender.com
```

Pair that with `LOCAL_HARNESS_URL=https://YOUR-HARNESS-SERVICE.onrender.com`
and `LOCAL_HARNESS_AUTH_TOKEN=<shared-secret>` on aura-os. The shared secret
authenticates only the transport hop; user/model auth still travels separately
inside `RuntimeRequest.auth_jwt`.

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

The workspace is organized into **ten layers** with strict downward-only dependencies (Invariant §13, enforced by [`tests/layer_boundary.rs`](tests/layer_boundary.rs)):

```text
core  <  store  <  config  <  model  <  context  <
plugin  <  exec  <  agent   <  fleet  <  surface
```

A crate may depend only on crates in the same layer or any lower layer. Every `crates/<crate>/src/lib.rs` carries a `//! Layer: <layer>` doc tag that must match the boundary test. There is **one** allowlisted carve-out today — `aura-tools → aura-kernel` (a Phase 10 follow-up to relocate the `Executor` / `ExecuteContext` / `SpawnHook` traits down to the exec layer).

### Crates by layer

All **58** workspace members, grouped by layer (lowest → highest). Crates marked `(shell)` are thin re-export shells that preserve historical import paths after a split/rename. Full per-crate reference (key types, modules) lives in [`docs/architecture.md`](docs/architecture.md).

**core** — behavior-free IDs, capability enums, mode primitives, wire types. No I/O, no async, no `aura-*` deps.

| Crate | Role |
|-------|------|
| [`aura-core-types`](crates/aura-core-types) | Identifier newtypes (`TurnId`, `RunId`, `ToolCallId`, `SessionId`) + share-by-value structs. |
| [`aura-core-modes`](crates/aura-core-modes) | Closed `AgentMode` enum, `ModeGate` / `ModeViolation`, `CapabilityProfile`, `KernelMode`. |
| [`aura-core-permissions`](crates/aura-core-permissions) | `Capability`, `Permissions`, `EffectivePermissions` + pure `narrow` / `intersect` / `effective` math. |
| [`aura-core-auth`](crates/aura-core-auth) | Auth primitive types: `AccessToken`, `RefreshToken`, `Token`, `StoredSession`, `AuthError` (data only). |
| [`aura-core-protocol`](crates/aura-core-protocol) | Wire-protocol primitives: `ProtocolVersion`, `PROTOCOL_VERSION`. |
| [`aura-core`](crates/aura-core) `(shell)` | Compatibility shell; still hosts larger domain types (`Transaction`, `Action`, `Effect`, `RecordEntry`, `ToolCall`, `AuraError`). |
| [`aura-protocol`](crates/aura-protocol) | Serde wire types for `POST /v1/run` (`RuntimeRequest`, `RuntimeRunResponse`, `InboundMessage`, `OutboundMessage`). Self-contained for external clients. |

**store** — durable storage. Owns the append-only record log + RocksDB column families. The record-append surface is sealed to `WriteStore` (Invariant §10).

| Crate | Role |
|-------|------|
| [`aura-store-db`](crates/aura-store-db) | RocksDB storage: column families, key encoders, atomic `WriteBatch` commit, sealed `WriteStore` + `ReadStore`. |
| [`aura-store-record`](crates/aura-store-record) | Backend-independent record types + `RecordLog` trait (`RecordEntry`, `RecordKind`, `SCHEMA_VERSION`). |
| [`aura-store-snapshot`](crates/aura-store-snapshot) | Content-addressed snapshot store trait for AuditedLite replay (`SnapshotStore`, `NoopSnapshotStore`). |
| [`aura-store`](crates/aura-store) `(shell)` | Re-export shell over `aura-store-db`. |

**config** — single source of truth for env vars + TOML config.

| Crate | Role |
|-------|------|
| [`aura-config`](crates/aura-config) | `AuraConfig` aggregate + `AgentConfig` / `ReasonerConfig` / `FleetConfig` (`default_mode`) + env loader. Hosts the `aura migrate` stub. |

**model** — LLM provider abstraction. One real provider, always proxy-routed (Invariant §1: only the kernel gateway may hold it in production).

| Crate | Role |
|-------|------|
| [`aura-model-reasoner`](crates/aura-model-reasoner) | `ModelProvider` trait, `ModelRequest` / `ModelResponse`, `StreamEvent` / `StreamAccumulator`, proxy-routed `AnthropicProvider`, `MockProvider`. |
| [`aura-reasoner`](crates/aura-reasoner) `(shell)` | Re-export shell over `aura-model-reasoner`. |

**context** — read-only context assembly: pulls signal *into* the prompt without side effects.

| Crate | Role |
|-------|------|
| [`aura-context-prompts`](crates/aura-context-prompts) | Render-only model-facing strings: system prompts, bootstrap, steering injections, recovery prompts. |
| [`aura-context-memory`](crates/aura-context-memory) | Per-agent memory: facts / events / procedures, two-stage write pipeline, deterministic retrieval, consolidation. |
| [`aura-context-compaction`](crates/aura-context-compaction) | Pure compaction: history tiers, redaction, cached tool-result summaries, `SummaryInput` / `SummaryOutput`. |
| [`aura-context-skills`](crates/aura-context-skills) | `SKILL.md` loader/registry/manager + `SkillInstallStore` (Claude Code `AgentSkills`-compatible). |
| [`aura-prompts`](crates/aura-prompts) `(shell)` | Re-export shell over `aura-context-prompts`. |
| [`aura-memory`](crates/aura-memory) `(shell)` | Re-export shell over `aura-context-memory`. |
| [`aura-compaction`](crates/aura-compaction) `(shell)` | Re-export shell over `aura-context-compaction`. |
| [`aura-skills`](crates/aura-skills) `(shell)` | Re-export shell over `aura-context-skills`. |

**plugin** — plugin runtime: contributor API + on-disk manifest pipeline + runtime surfaces (hooks, MCP, connectors). Sandboxed (Invariant §14).

| Crate | Role |
|-------|------|
| [`aura-plugin-api`](crates/aura-plugin-api) | In-process contributor trait surface (`PluginContributor`, `ContributionKind`, `PluginRoot`). |
| [`aura-plugin-core`](crates/aura-plugin-core) | Declarative manifest schema, install pipeline, cache layout, marketplace + trust-prompt flow. |
| [`aura-plugin-hooks`](crates/aura-plugin-hooks) | Hook engine: closed `HookEvent` taxonomy, `HookEngine`, `HookOutcome`, sandboxed env scrubbing. |
| [`aura-plugin-mcp`](crates/aura-plugin-mcp) | Stdio MCP JSON-RPC client + first-active-wins connection manager. |
| [`aura-plugin-connectors`](crates/aura-plugin-connectors) | Thread-safe registry of plugin-contributed external endpoints (last-wins). |

**exec** — tool execution surface: catalog down through sandbox, conflict locks, worktree isolation.

| Crate | Role |
|-------|------|
| [`aura-exec-conflict`](crates/aura-exec-conflict) | Domain-scoped advisory locks (`ConflictRegistry`, `LockHandle`) to reduce sibling collisions. |
| [`aura-exec-isolation`](crates/aura-exec-isolation) | Subagent workspace isolation: `WorktreeIsolation` (preferred) + `CopyIsolation` fallback. |
| [`aura-exec-policy`](crates/aura-exec-policy) | Pure capability-satisfaction `evaluate` over `EffectivePermissions` (not a substitute for `Policy::check`). |
| [`aura-exec-sandbox`](crates/aura-exec-sandbox) | OS containment primitives: `FsSandbox` (path/symlink guard) + `ProcessSandbox`. |
| [`aura-exec-tools`](crates/aura-exec-tools) `(shell)` | Re-export shell over `aura-tools` + `sandbox` / `policy` sub-modules. |
| [`aura-exec-runner`](crates/aura-exec-runner) `(shell)` | Layered alias for `ToolExecutor` + `conflict` / `isolation` re-exports. |
| [`aura-tools`](crates/aura-tools) | Tool catalog, resolver, sandboxed FS/command tools, git tools, domain tools, `task` tool, automaton tools. Implements `Executor`. |

**agent** — the deterministic core of a single agent. Invariants §1–§11 are owned here.

| Crate | Role |
|-------|------|
| [`aura-agent-kernel`](crates/aura-agent-kernel) | The deterministic kernel: context build, reason, policy, `ExecutorRouter`, `RecordEntry` production, replay. |
| [`aura-agent-loop`](crates/aura-agent-loop) `(shell)` | Re-export shell exposing `AgentLoop`, `AgentLoopConfig`, `TurnEvent`, `RunOptions`. |
| [`aura-agent-steering`](crates/aura-agent-steering) | Per-turn steering evaluators (`RepeatedReadTracker`, `ImplementNow`, `EarlyOracle`) + `SteeringRegistry`. |
| [`aura-agent-subagent`](crates/aura-agent-subagent) | Subagent derivation/inheritance/registry + pure-data dispatcher adapters (`derive_subagent`, may only narrow). |
| [`aura-agent`](crates/aura-agent) | Multi-step orchestration loop + kernel gateways (`KernelModelGateway`, `KernelToolGateway`, `KernelDomainGateway`) + sealed `RecordingModelProvider`. |
| [`aura-kernel`](crates/aura-kernel) `(shell)` | Re-export shell over `aura-agent-kernel`. |

**fleet** — the multi-agent runtime above the single-agent kernel. Invariant §12 / §15 live here.

| Crate | Role |
|-------|------|
| [`aura-fleet-registry`](crates/aura-fleet-registry) | In-memory directory of live/terminated agents (`FleetRegistry`, `AgentSlot`, `AgentState`). |
| [`aura-fleet-quota`](crates/aura-fleet-quota) | Concurrency/resource budgets: `QuotaPool` + RAII `BudgetTicket`. |
| [`aura-fleet-spawn`](crates/aura-fleet-spawn) | Spawn pipeline: dedupe, per-parent audit lease (`ParentLeaseRegistry`), quota, orphan handoff (`OrphanStore`). |
| [`aura-fleet-dispatch`](crates/aura-fleet-dispatch) | Routes `AgentJob` items into `FleetSpawner::spawn`. |
| [`aura-fleet-mailbox`](crates/aura-fleet-mailbox) | Bounded MPSC mailbox of `AgentJob` with backpressure + typed send errors. |
| [`aura-fleet-daemon`](crates/aura-fleet-daemon) | Composition root wiring registry/spawner/dispatcher/quota/mailbox; hosts `resolve_session_mode`. |
| [`aura-fleet-subagent`](crates/aura-fleet-subagent) | Concrete `FleetSubagentDispatcher` impl of `SubagentDispatchHook` (composes spawn + registry + quota + `ChildRunner`). |

**surface** — composition roots: each assembles lower layers into a runnable entry point or side-effectful client.

| Crate | Role |
|-------|------|
| [`aura-surface-cli`](crates/aura-surface-cli) | CLI composition root: clap `Cli` / `Commands` / `RunArgs`, `ModeFlag`, event loop, session helpers. |
| [`aura-surface-sdk`](crates/aura-surface-sdk) | External SDK types (`AuraClient`, `AuraSession`, `SessionConfig.mode`); pluggable transport. |
| [`aura-surface-terminal`](crates/aura-surface-terminal) `(shell)` | Shell over `aura-terminal` + typed `SlashModeCommand`. |
| [`aura-surface-automaton`](crates/aura-surface-automaton) `(shell)` | Shell over `aura-automaton`. |
| [`aura-surface-auth`](crates/aura-surface-auth) `(shell)` | Shell for zOS HTTP client + credential storage (`ZosClient`, `CredentialStore`). |
| [`aura-runtime`](crates/aura-runtime) | HTTP/WS **gateway** crate + `aura-node` composition root. **Sole Cargo surface** for external Rust consumers. |
| [`aura-engine`](crates/aura-engine) | Orchestration engine: per-agent `Scheduler`, worker, `AutomatonBridge`, `MemoryTurnObserver`, `RuntimeChildRunner`. |
| [`aura-domain-http`](crates/aura-domain-http) | HTTP `DomainApi` impl (`HttpDomainApi`) + JWT-injecting wrapper (`JwtDomainApi`). |
| [`aura-terminal`](crates/aura-terminal) | Ratatui TUI library: themed rendering, components, input handling, `App` state machine. |
| [`aura-automaton`](crates/aura-automaton) | Long-running automatons: `ChatAutomaton`, `DevLoopAutomaton`, `SpecGenAutomaton`, `TaskRunAutomaton`. |
| [`aura-auth`](crates/aura-auth) | zOS login client + credential persistence (keyring + `~/.aura/credentials.json`). |

> **External-consumer invariant:** [`aura-runtime`](crates/aura-runtime) is the sole Cargo surface for any external Rust consumer. External repos interact with the harness over the wire (`POST /v1/run`, `WS /stream/:run_id`, and the management endpoints) — never by depending on `aura-engine`, `aura-domain-http`, or any other lower-layer crate directly.

All members are declared in [`Cargo.toml`](Cargo.toml) under `[workspace].members`.

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
                    │  store-backed per-agent claim (Invariant §12.a) │
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
                    │  Build context  ·  AgentMode gate  ·  Policy    │
                    │  Call Reasoner  ·  Execute actions              │
                    │  Build RecordEntry (sealed WriteStore)          │
                    └─────┬──────────────────┬──────────────┬────────┘
                          │                  │              │
             ┌────────────▼─────┐  ┌─────────▼────┐  ┌─────▼──────────┐
             │     Reasoner     │  │   Executor   │  │     Store      │
             │ KernelModelGate  │  │   (Tools)    │  │   (RocksDB)    │
             │       way        │  │  FS · Cmd    │  │  record        │
             │   (records every │  │  Domain      │  │  agent_meta    │
             │     LLM call)    │  │  Automaton   │  │  inbox         │
             └──────┬───────────┘  └──────────────┘  │  memory_*      │
                    │                                │  agent_skills  │
                    │                                └────────────────┘
      ┌─────────────▼──────────────┐   ┌───────────────────────────────┐
      │  aura-router (proxy, JWT)   │   │     Domain Services           │
      │  the only LLM egress —      │   │  Orbit · Aura Storage         │
      │  no direct provider path    │   │  Aura Network                 │
      └──────────────┬──────────────┘   └───────────────────────────────┘
                     │
              ┌──────▼───────┐
              │  Upstream     │
              │  providers    │
              │ (Anthropic …) │
              └───────────────┘
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

### Tool permissions & defaults

Tri-state (`on` / `off` / `ask`) tool configuration consumed by the policy gate (Invariant §4). PUTs append a kernel-built record entry serialized through the per-agent scheduler claim (Invariant §12.a).

| Method | Path | Purpose |
|--------|------|---------|
| GET, PUT | `/users/:user_id/tool-defaults` | Read / write the user's `UserToolDefaults` baseline. |
| GET, PUT | `/agents/:agent_id/tool-permissions` | Read / write per-agent `AgentToolPermissions` overrides. |
| GET | `/agents/:agent_id/tools` | Resolved tool list for an agent (catalog ∩ permissions). |

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

Canonical routes are mounted under `/memory/...`; compatibility aliases are mounted under `/api/agents/:agent_id/memory/...` (the `aura-os` proxy uses the alias form). Both surfaces cover:

- Facts: list / create / update / delete, fetch by key.
- Events: list / create / delete, bulk-delete.
- Procedures: list / create / update / delete.
- `GET /memory/:agent_id/snapshot` — full memory snapshot.
- `POST /memory/:agent_id/wipe` — clear all memory for an agent.
- `GET /memory/:agent_id/stats` — counts, token budgets.
- `POST /memory/:agent_id/consolidate` — trigger consolidation.

The alias router also exposes `GET /api/agents/:agent_id/memory` (snapshot) and `DELETE /api/agents/:agent_id/memory` (wipe) as a combined route.

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

Writes flow through a two-stage pipeline (heuristic extractor + optional LLM refiner, see [`crates/aura-context-memory/src/write_pipeline.rs`](crates/aura-context-memory/src/write_pipeline.rs) and [`crates/aura-context-memory/src/refinement.rs`](crates/aura-context-memory/src/refinement.rs)). `MemoryRetriever` injects a size-budgeted slice of memory into the kernel context on each turn.

## Skills

`aura-skills` loads `SKILL.md` packages from (in precedence order):

1. Workspace — `{workspace}/skills/`
2. Agent-personal — `~/.aura/agents/{id}/skills/`
3. Personal — `~/.aura/skills/`
4. Extra directories from config
5. Bundled skills shipped with the runtime

`SkillManager` exposes activation and prompt injection; `SkillInstallStore` persists per-agent installs in the `AGENT_SKILLS` column family. See [`crates/aura-context-skills/src/lib.rs`](crates/aura-context-skills/src/lib.rs).

## Configuration

The node reads configuration from environment variables via `NodeConfig::from_env()` in [`crates/aura-runtime/src/config/mod.rs`](crates/aura-runtime/src/config/mod.rs). Copy [`.env.example`](.env.example) as a starting point.

### LLM routing

All LLM traffic flows through the AURA router (proxy) using a per-request JWT. There is no direct-provider path: `aura-harness` does not call Anthropic (or any other provider) on its own.

| Variable | Default | Description |
|----------|---------|-------------|
| `AURA_ROUTER_URL` | `https://aura-router.onrender.com` | Proxy router endpoint. |
| `AURA_ROUTER_JWT` | — | JWT for terminal/CLI sessions. WebSocket clients supply their own. |
| `AURA_DEFAULT_MODEL` | `claude-opus-4-6` (`aura_model_reasoner::ENV_FALLBACK_MODEL`) | Model identifier sent to the router **only** when the request did not pin a model itself; sessions, dev-loop runs, and task runs all carry an explicit model end-to-end. (Legacy `AURA_ANTHROPIC_MODEL` is still read as a fallback for one release.) |
| `AURA_DEFAULT_FALLBACK_MODEL` | — | Optional secondary model used on 429/529 retries. |
| `AURA_MODEL_TIMEOUT_MS` | `300000` | LLM request timeout (resolved by `AnthropicConfig::from_env`). |
| `AURA_GENERATION_STREAM_IDLE_TIMEOUT_SECS` | `300` | Maximum quiet window while proxying generation SSE bytes from the router before emitting `STREAM_IDLE_TIMEOUT`. |
| `AURA_LLM_MAX_RETRIES` | `8` | Per-model retry budget before falling back. |
| `AURA_DISABLE_PROMPT_CACHING` | — | Set to `1`/`true`/`yes` to disable Anthropic prompt-caching directives. |

Additional LLM tuning knobs (all optional, read by `AnthropicConfig::from_env`): `AURA_LLM_BACKOFF_INITIAL_MS` (`250`), `AURA_LLM_BACKOFF_CAP_MS` (`30000`), `AURA_LLM_MIN_REQUEST_INTERVAL_MS` (`0` = disabled), `AURA_LLM_EMERGENCY_BODY_CAP_BYTES` (`524288`; `0` disables), `AURA_LLM_CLOUDFLARE_MAX_RETRIES` (`3`).

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
| `AURA_OS_SERVER_URL` (alias `AURA_SERVER_BASE_URL`) | auto `http://127.0.0.1:19847` on loopback binds, else — | Routes spec/task/project/log writes (and the cross-agent `send_to_agent` hook) through `aura-os-server`. |
| `AURA_ALLOW_UNRESTRICTED_FULL_ACCESS` | `false` | Operator ceiling permitting effective-FullAccess sessions to bypass command allowlists (`1`/`true` to enable). Mirrors the `--allow-unrestricted-full-access` CLI flag. |
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
