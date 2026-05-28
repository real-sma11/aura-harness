# Aura Harness — Architecture

This document is organized in three parts:

1. **Part 0 — Layer overview.** The ten-layer stack the workspace ships with today, the strict downward-only dependency rule, and the `AgentMode` resolution chain.
2. **Part 1 — Layered crate reference.** One section per layer, with a per-layer overview followed by a sub-section for every crate (purpose, key types, key modules).
3. **Part 2 — User flows.** Sequence diagrams showing how data moves through the system for interactive, headless, WebSocket, and error-recovery paths.

---

## Part 0 — Layer overview

The architecture refactor (Phases 1 → 9, see [CHANGELOG.md](../CHANGELOG.md)) split the workspace into **ten layers**. Crates are named `aura-<layer>-<name>` and may depend only on crates whose layer is the same or lower. The boundary is enforced by [tests/layer_boundary.rs](../tests/layer_boundary.rs) and a per-crate `//! Layer: <layer>` doc-comment audit.

```text
core  <  store  <  config  <  model  <  context  <
plugin  <  exec  <  agent   <  fleet  <  surface
```

### Layers, in dependency order

| Layer    | Purpose                                                                          | Crates                                                                                                                                                                                       |
|----------|----------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| core     | Behavior-free IDs, capability enums, mode primitives, wire types.                | `aura-core-types`, `aura-core-modes`, `aura-core-permissions`, `aura-core-auth`, `aura-core-protocol`, `aura-core` (shell), `aura-protocol`.                                                  |
| store    | Durable storage: agent state, append-only audit log, snapshot I/O.               | `aura-store-db`, `aura-store-record`, `aura-store-snapshot`, `aura-store` (shell).                                                                                                            |
| config   | Single source of truth for env vars + TOML config.                               | `aura-config`.                                                                                                                                                                                |
| model    | LLM provider trait + streaming completions.                                      | `aura-model-reasoner`, `aura-reasoner` (shell).                                                                                                                                               |
| context  | Read-only context assembly (memory, skills, compaction, prompts).                | `aura-context-prompts`, `aura-context-memory`, `aura-context-compaction`, `aura-context-skills`, plus the `aura-{prompts,memory,compaction,skills}` shells.                                   |
| plugin   | Plugin manifest schema, in-process API, hooks, MCP, connectors.                  | `aura-plugin-api`, `aura-plugin-core`, `aura-plugin-hooks`, `aura-plugin-mcp`, `aura-plugin-connectors`.                                                                                       |
| exec     | Tool catalog, execution runner, sandbox, policy, isolation, conflict locks.      | `aura-exec-conflict`, `aura-exec-isolation`, `aura-exec-policy`, `aura-exec-sandbox`, `aura-exec-tools`, `aura-exec-runner`, `aura-tools`.                                                    |
| agent    | Single-agent turn loop + audited kernel + steering + subagent derivation.        | `aura-agent-kernel`, `aura-agent-loop`, `aura-agent-steering`, `aura-agent-subagent`, `aura-agent`, `aura-kernel` (shell).                                                                    |
| fleet    | Multi-agent registry, spawn, dispatch, quota, mailbox, daemon composition root, subagent dispatcher. | `aura-fleet-registry`, `aura-fleet-spawn`, `aura-fleet-dispatch`, `aura-fleet-quota`, `aura-fleet-mailbox`, `aura-fleet-daemon`, `aura-fleet-subagent`.                                       |
| surface  | Composition roots: CLI / TUI / SDK / automaton / auth / HTTP-WS gateway / orchestration engine / domain HTTP. | `aura-surface-cli`, `aura-surface-sdk`, `aura-surface-terminal`, `aura-surface-automaton`, `aura-surface-auth`, `aura-runtime`, `aura-engine`, `aura-domain-http`, `aura-terminal`, `aura-automaton`, `aura-auth`. |

### Dependency rules

- A crate may depend on crates in the same layer or any lower layer. Upward edges fail CI via [tests/layer_boundary.rs](../tests/layer_boundary.rs).
- Every `crates/<crate>/src/lib.rs` carries a `//! Layer: <layer>` doc-comment that must match the `KNOWN_CRATES` table in the boundary test (audited by `every_crate_carries_a_matching_layer_doc_tag`).
- **One documented exception** remains: `aura-tools -> aura-kernel` is allowlisted as a Phase 10 follow-up (the deep fix is to relocate `Executor` / `ExecuteContext` / `SpawnHook` traits from the agent layer to the exec layer). See `WARN_ONLY_UPWARD_EDGES` in [tests/layer_boundary.rs](../tests/layer_boundary.rs).

### Layered dependency diagram

Each box is a layer. Arrows point in the only allowed dependency direction: **downward**. A crate at any layer may depend on crates at the same layer or any layer below, never above.

```text
   ┌────────────────────────────────────────────────────────────────┐
   │  surface — CLI · TUI · SDK · HTTP/WS gateway · orchestration   │
   │  engine · domain HTTP · automaton · auth                       │
   │  aura-surface-cli, aura-surface-sdk, aura-surface-terminal,    │
   │  aura-surface-automaton, aura-surface-auth, aura-runtime,      │
   │  aura-engine, aura-domain-http, aura-terminal, aura-automaton, │
   │  aura-auth                                                     │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  fleet — multi-agent registry · spawn · dispatch · quota ·     │
   │  mailbox · daemon composition root · subagent dispatcher       │
   │  aura-fleet-{registry,spawn,dispatch,quota,mailbox,daemon,     │
   │              subagent}                                         │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  agent — deterministic kernel · AgentLoop · steering ·         │
   │  subagent derivation                                           │
   │  aura-agent-{kernel,loop,steering,subagent}, aura-agent,       │
   │  aura-kernel (shell)                                           │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  exec — tool catalog · runner · sandbox · policy ·             │
   │  isolation · conflict locks                                    │
   │  aura-exec-{conflict,isolation,policy,sandbox,tools,runner},   │
   │  aura-tools                                                    │
   │  ╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌  │
   │  ⚠ warn-only upward edge: aura-tools  ─ ─ ─▶  aura-kernel      │
   │     (Phase 10: relocate Executor traits to the exec layer)     │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  plugin — manifest schema · in-process API · hooks · MCP ·     │
   │  connectors                                                    │
   │  aura-plugin-{api,core,hooks,mcp,connectors}                   │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  context — read-only prompt assembly · memory · compaction ·   │
   │  skills                                                        │
   │  aura-context-{prompts,memory,compaction,skills} (+ shells)    │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  model — LLM provider trait · streaming completions            │
   │  aura-model-reasoner, aura-reasoner (shell)                    │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  config — env vars + TOML config (single source of truth)      │
   │  aura-config                                                   │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  store — durable storage · sealed WriteStore (Invariant §10)   │
   │  aura-store-{db,record,snapshot}, aura-store (shell)           │
   └─────────────────────────────┬──────────────────────────────────┘
                                 │
   ┌─────────────────────────────▼──────────────────────────────────┐
   │  core — behavior-free IDs · capability enums · modes · wire    │
   │  aura-core-{types,modes,permissions,auth,protocol},            │
   │  aura-core (shell), aura-protocol                              │
   └────────────────────────────────────────────────────────────────┘
```

### `AgentMode` resolution priority (Phase 9)

`AgentMode` (`Agent` / `Plan` / `Ask` / `Debug`) is the headline gate consulted before every external effect — it runs **before** the policy layer's permission check, not as a substitute for it. The resolution order at session start is:

1. **CLI flag** — `aura --mode <agent|plan|ask|debug>` (clap `ModeFlag` from `aura-surface-cli`).
2. **TUI slash command** — `/mode <agent|plan|ask|debug>` parsed by `aura_surface_terminal::SlashModeCommand`.
3. **SDK field** — `aura_surface_sdk::SessionConfig::mode`.
4. **Daemon default** — `aura_config::FleetConfig::default_mode` (overridable via `AURA_FLEET_DEFAULT_MODE`).
5. **Fallback** — `AgentMode::Agent`.

`aura_fleet_daemon::resolve_session_mode` consumes an `AgentModeInputs` and applies the priority deterministically. The result is recorded on the session and propagated to every child agent through `aura-agent-subagent::derive_subagent` (children may only narrow, never widen).

### Foreground subagents

The v1 subagent model is foreground and local to one harness instance. A parent agent calls the `task` tool, which validates `Capability::SpawnAgent` and hands an `aura-core::SubagentDispatchRequest` to a `SubagentDispatchHook`. The tool is fail-closed when that hook is absent.

After the Phase B engine extraction, subagent dispatch is split across three layers — every edge stays downward, so no new `WARN_ONLY_UPWARD_EDGES` allowlist entry is needed:

- **[`aura-tools`](../crates/aura-tools)** (exec) declares the `SubagentDispatchHook` trait that the `task` tool consumes. Unchanged.
- **[`aura-agent-subagent`](../crates/aura-agent-subagent)** (agent) owns the pure data + transforms: `SubagentRegistry` + bundled kinds (`registry.rs`) and the dispatcher's pure-data adapter helpers (`adapters.rs`: `parent_context_from_request`, `overrides_from_request`, `narrow_permissions`, `legacy_permissions_to_modes`, `core_to_modes_*`). No fleet deps.
- **[`aura-fleet-subagent`](../crates/aura-fleet-subagent)** (fleet) owns the concrete `FleetSubagentDispatcher` impl of `SubagentDispatchHook`. It composes `aura-fleet-spawn::FleetSpawner` + `aura-fleet-registry` + `aura-fleet-quota` with the agent-layer registry and the surface-layer `ChildRunner`.
- **[`aura-engine`](../crates/aura-engine)** (surface) owns the `RuntimeChildRunner` impl of [`aura_fleet_spawn::ChildRunner`] — the only surface-layer piece. It drives the engine's per-agent `Scheduler` so child work inherits the same per-agent claim and record pipeline as ordinary scheduled work.

The composition root in [`aura-runtime/src/node.rs`](../crates/aura-runtime/src/node.rs) wires the four together: build an `Engine`, pull its `Arc<dyn ChildRunner>` out, construct a `FleetSubagentDispatcher` over the engine's child-runner + the agent layer's registry + the fleet's spawner/quota/registry, then hand the resulting `Arc<dyn SubagentDispatchHook>` to the gateway. Parent delegation is serialized through the per-parent `ParentLeaseRegistry` lease before the parent tool batch commits, avoiding races between parallel `task` calls.

### External-consumer invariant

[`aura-runtime`](../crates/aura-runtime) is the **sole Cargo surface** for any external Rust consumer of the harness. External repos (`aura-os`, any future SDK consumer) interact with the harness exclusively over the wire — `POST /v1/run` for submission, `WS /stream/:run_id` for events, the management endpoints (skills, memory, tool defaults, files, tx) for everything else. No external `Cargo.toml` may depend on `aura-engine`, `aura-domain-http`, `aura-agent-subagent`, `aura-fleet-subagent`, `aura-kernel`, or any other lower-layer crate. If a forward-looking case ever needs in-process types, they must be re-exported through `aura-runtime` — never reached for directly. This rule is load-bearing: it lets `aura-runtime`'s composition root evolve internally without dragging every external consumer through a coordinated migration. Cross-reference: the §Surface layer entry for [`aura-runtime`](#aura-runtime) repeats the rule, and the `aura-runtime gateway refactor` plan's "External-consumer constraint" section is its canonical statement.

---

## Part 1 — Layered crate reference

Each section opens with a paragraph describing the layer's purpose and the invariants it owns, then walks every crate at that layer. Per-crate entries cite the canonical files via markdown links so they can be opened directly in the editor.

---

### Core layer

The foundation. Behavior-free crates that define the IDs, capability enums, modes, and wire types that every other layer reaches for. No I/O, no async runtime, no aura-* dependencies. Splitting these out of the legacy `aura-core` was Phase 1 of the refactor; the original crate stays as a re-export shell so historical import paths keep compiling.

#### [`aura-core-types`](../crates/aura-core-types)

Strongly-typed identifier newtypes (`TurnId`, `RunId`, `ToolCallId`, `SessionId`) and the small share-by-value structs that the agent/fleet layers traffic in. Re-exports `AgentMode` and `Capability` for crates that want a single import surface.

#### [`aura-core-modes`](../crates/aura-core-modes)

Closed `AgentMode` enum (`Agent`, `Plan`, `Ask`, `Debug`) plus the `ModeGate` and `ModeViolation` primitives consulted before every external effect. Also owns `CapabilityProfile`, the per-mode capability mask the policy layer narrows against.

#### [`aura-core-permissions`](../crates/aura-core-permissions)

Privilege types (`Capability`, `Permissions`, `EffectivePermissions`) and the pure resolution math — `narrow`, `intersect`, `effective` — used by both the kernel's policy gate and `aura-exec-policy`. Pure functions only; no I/O or config reads.

#### [`aura-core-auth`](../crates/aura-core-auth)

Auth primitive types: `AccessToken`, `RefreshToken`, `Token`, `StoredSession`, `AuthError`. The surface-layer `aura-auth` / `aura-surface-auth` shells provide the keyring and HTTP implementations; this crate is data only.

#### [`aura-core-protocol`](../crates/aura-core-protocol)

Phase 1 wire-protocol primitives — currently just `ProtocolVersion` and `PROTOCOL_VERSION`. Used by the SDK and the WS handshake to negotiate compatible versions.

#### [`aura-core`](../crates/aura-core) (shell)

Phase 1 compatibility shell. Re-exports the split core crates and still hosts the larger domain types (`Transaction`, `TransactionType`, `Action`, `ActionKind`, `Effect`, `RecordEntry`, `ToolCall`, `ToolResult`, `Identity`, `AuraError`) that have not yet been moved to a more specific home. Most workspace crates can simply `use aura_core::*`.

#### [`aura-protocol`](../crates/aura-protocol)

Serde types for the wire API consumed by `aura-runtime` and external clients (e.g. the `aura-os` web UI). After the Phase A wire-shape unification this crate owns:

- **`RuntimeRequest`** — canonical `POST /v1/run` body. Fields: `r#type` (a `RuntimeRequestType` discriminated union over `Chat { conversation_messages }` / `DevLoop {}` / `TaskRun { task_id, prior_failure, work_log }`), `agent_identity` (an `AgentIdentity` wrapper carrying `template_id`, `partition_id`, `persona`, `skills`, `system_prompt`), `model` (a `ModelSelection`: id, max_tokens, max_turns, temperature, provider_overrides), `workspace` (a `WorkspaceLocation`), `project` (an `Option<ProjectContext>`), `agent_permissions` (kernel-enforced policy bundle), `tool_permissions` (per-tool on/off overrides), `agent_capabilities` (runtime-resolved tools / integrations / classifier the agent **can** use), `auth_jwt: Option<String>`, `user_id`.
- **`RuntimeRunResponse`** — synchronous body returned by `POST /v1/run`: `{ run_id, event_stream_url }`. The caller then opens `WS /stream/:run_id`.
- **`AgentPersona`** — the narrow `{ name, role, personality }` triple nested as the `persona` field of `AgentIdentity`. Renamed from the pre-Phase-A `AgentIdentityWire` to free the `AgentIdentity` name for the wider wrapper.
- **`InboundMessage`** — `UserMessage`, `Cancel`, `ApprovalResponse`, `ToolApprovalResponse`, `GenerationRequest`. The pre-Phase-A `InboundMessage::SessionInit` first-frame variant is **deleted**; runs are now started over HTTP before the WS opens.
- **`OutboundMessage`** — `SessionReady` (still the first server-emitted frame), `AssistantMessageStart`, `TextDelta`, `ThinkingDelta`, `ToolUseStart`, `ToolResult`, `AssistantMessageEnd`, `Error`, plus the generation-proxy progress family.

**Naming rationale** (kept here so future contributors don't reopen the question):

- `agent_permissions` vs `agent_capabilities` — permissions decide whether a capability is **allowed** (kernel-enforced policy gate); capabilities decide what concrete tools / integrations / classifier materialize it (runtime resolution). The "agent_" prefix qualifies the noun so neither field is confused with `aura_core_permissions::Capability` (the privilege enum).
- `auth_jwt: Option<String>` rather than a struct wrapper — the credential shape is a bare bearer JWT today, and the field stays a plain `Option<String>` so the wire surface doesn't pretend to support polymorphic credential types it doesn't actually have. If richer auth ever lands it gets its own field rather than a typed `AuthCredentials` enum.

`SessionInit`, `AutomatonStartRequest`, and the pre-rename `AgentIdentityWire` are all **removed** with no aliases. External consumers of `aura-protocol` (e.g. `aura-os`) mirror the same shape in their own kept-in-sync copy of this crate. Self-contained so external clients can depend on it without pulling in the runtime.

---

### Store layer

Durable persistence. Owns the append-only record log and all RocksDB column families. **Invariant §10** lives here: the record-append surface (`append_entry_atomic`, `append_entries_batch`, …) lives on the sealed `WriteStore` trait, so only the kernel's `Arc<dyn WriteStore>` can commit a record entry. Non-kernel callers depend on `Arc<dyn ReadStore>`.

#### [`aura-store-db`](../crates/aura-store-db)

RocksDB-backed durable storage. Owns the three column families (`record`, `agent_meta`, `inbox`), the `RecordKey` / `AgentMetaKey` / `InboxKey` encoders, and the atomic `WriteBatch` commit path. Implements both `Store` (legacy) and the sealed `WriteStore` (Phase 2 split).

#### [`aura-store-record`](../crates/aura-store-record)

Append-only domain types and the `RecordLog` trait contract independent of any storage backend. Defines `RecordEntry`, `RecordKind`, and `RecordLogError`. Other layers consume this rather than `aura-store-db` so the backend can be swapped without touching them.

#### [`aura-store-snapshot`](../crates/aura-store-snapshot)

Content-addressed snapshot store trait (`SnapshotStore`, `SnapshotError`, `NoopSnapshotStore`). V1 ships a no-op stub; future replay/audit work plugs in a real implementation here without touching call sites.

#### [`aura-store`](../crates/aura-store) (shell)

Re-export shell over `aura-store-db` so legacy `aura_store::*` imports keep compiling unchanged.

---

### Config layer

Configuration loading and the resolved configuration types. The single source of truth for env vars and TOML config — every other crate reaches `aura_config::loaded()` rather than calling `std::env::var` directly.

#### [`aura-config`](../crates/aura-config)

`AuraConfig` aggregate plus the per-subsystem `AgentConfig`, `ReasonerConfig`, `FleetConfig` (carries `default_mode` — the daemon rung of the `AgentMode` resolution chain), and the env loader (`env::AURA_HOME`, `AURA_FLEET_DEFAULT_MODE`, retry/thinking budgets). Hosts the `aura migrate` stub (Phase 4a).

---

### Model layer

LLM provider abstraction. Defines the `ModelProvider` trait, normalized message and stream types, and the (single) Anthropic-shaped router/proxy client. **Invariant §1** lives here: only `KernelModelGateway` (in the agent layer) may hold a `ModelProvider` for production code paths — automatons take `P: aura_agent::RecordingModelProvider`, a sealed marker trait that only the recording gateway implements.

#### [`aura-model-reasoner`](../crates/aura-model-reasoner)

`ModelProvider` trait (`complete`, `complete_streaming`, `health_check`), `ModelRequest` / `ModelResponse` shapes, `Message` / `ContentBlock` / `StopReason`, the `StreamEvent` SSE family and `StreamAccumulator`, the proxy-routed `AnthropicProvider` (with retry + model-chain fallback), and `MockProvider` for tests. The Anthropic SSE parser is split into `anthropic/sse/{parse,event,state}.rs` for readability.

#### [`aura-reasoner`](../crates/aura-reasoner) (shell)

Re-export shell over `aura-model-reasoner` for source-compatible imports.

---

### Context layer

Read-only context assembly. Everything that pulls signal *into* the prompt without producing side effects: prompt rendering, per-agent memory, message-history compaction, skill packages. Each surface has a `aura-context-<name>` crate at this layer plus a legacy `aura-<name>` re-export shell.

#### [`aura-context-prompts`](../crates/aura-context-prompts)

Render-only construction of every model-facing string: system prompts, bootstrap blocks, steering injections, error-recovery fix prompts. Notable types: `SystemPromptBuilder`, `bootstrap`, `SteeringRenderer`, `descriptors`. Has no provider or store deps.

#### [`aura-prompts`](../crates/aura-prompts) (shell)

Re-export shell over `aura-context-prompts`.

#### [`aura-context-memory`](../crates/aura-context-memory)

Per-agent long-term memory: fact storage, episodic events, procedural pattern detection, a two-stage write pipeline (heuristic extraction → optional LLM refinement), deterministic retrieval for prompt injection, and consolidation. `MemoryManager` is the facade embedders use. Stores live in dedicated column families (`MEMORY_FACTS`, `MEMORY_EVENTS`, `MEMORY_PROCEDURES`) — never the record log.

Key types: `MemoryManager`, `MemoryStore` / `MemoryStoreApi`, `MemoryWritePipeline`, `MemoryRetriever`, `MemoryConsolidator`, `TurnSummary`, `MemoryPacket`. Phase 6c inverted this crate's dependency on `aura-agent` by injecting a `ModelProvider` and accepting `TurnSummary` rather than calling back into the agent loop.

#### [`aura-memory`](../crates/aura-memory) (shell)

Re-export shell over `aura-context-memory`.

#### [`aura-context-compaction`](../crates/aura-context-compaction)

Unified pure compaction: message-history tier selection, pressure-gated write/edit input redaction, structured `_redacted` markers, cached tool-result summaries, tool-surface compaction, storage compaction, and the `SummaryInput` / `SummaryOutput` data used for summary escalation. Does not call a model itself — `aura-agent` performs the model call and applies the result through this crate.

#### [`aura-compaction`](../crates/aura-compaction) (shell)

Re-export shell over `aura-context-compaction`.

#### [`aura-context-skills`](../crates/aura-context-skills)

Skill system wire-compatible with the Claude Code `SKILL.md` / `AgentSkills` open standard. Loader precedence: workspace → agent-personal → personal → extra dirs → bundled. `SkillManager` exposes activation and prompt-injection; `SkillInstallStore` persists per-agent installs in the `AGENT_SKILLS` column family.

Key types: `SkillLoader`, `SkillRegistry`, `SkillManager`, `SkillInstallStore`, `SkillActivation`, `AgentSkillPermissions`.

#### [`aura-skills`](../crates/aura-skills) (shell)

Re-export shell over `aura-context-skills`.

---

### Plugin layer

The plugin runtime. Splits into a contributor API surface (first-party plugins shipped in-tree) and an on-disk manifest / install / cache / marketplace pipeline. Phase 4c added the runtime surfaces — hooks, MCP, connectors — that Phase 8 then wired into the agent loop and fleet spawner.

#### [`aura-plugin-api`](../crates/aura-plugin-api)

In-process contributor trait surface for first-party plugins. `PluginContributor`, `ContributionKind`, `PluginRoot`, `PluginId`. Not a dynamic loader — plugins are compiled in and registered at startup.

#### [`aura-plugin-core`](../crates/aura-plugin-core)

Declarative manifest schema, install pipeline, cache layout under `AURA_HOME/plugins/`, and marketplace lookup. Owns `PluginManifest`, `install`, `marketplace`, and the trust-prompt flow. Consumed by `aura plugins install/list/enable/disable`.

#### [`aura-plugin-hooks`](../crates/aura-plugin-hooks)

Hook engine: closed `HookEvent` taxonomy (10 Codex/Claude-aligned lifecycle events), `HookEngine`, `HookOutcome`, and the sandboxed env scrubbing for hook commands. Hooks fire at every documented agent-loop and fleet-spawn lifecycle point.

#### [`aura-plugin-mcp`](../crates/aura-plugin-mcp)

Stdio MCP JSON-RPC client and a first-active-wins connection manager keyed by server id. Notable types: `McpClient`, `McpConnectionManager`, `ServerConfig`, `McpError`. Phase 8 wires MCP servers into the tool catalog.

#### [`aura-plugin-connectors`](../crates/aura-plugin-connectors)

Thread-safe registry of plugin-contributed external endpoints. `ConnectorRegistry`, `ConnectorEntry`, `ConnectorError`. Last-wins registration semantics covered by [crates/aura-plugin-connectors/tests/last_wins.rs](../crates/aura-plugin-connectors/tests/last_wins.rs).

---

### Exec layer

Tool execution surface. Everything from the tool catalog down through sandbox primitives, conflict locks, and worktree isolation. The Phase 5 split broke the monolithic `aura-tools` crate into five companions; the legacy crate remains as the catalog of built-in tools.

> **Warn-only edge:** `aura-tools -> aura-kernel` is the single remaining upward dependency in the workspace. The deep fix is to relocate `Executor` / `ExecuteContext` / `SpawnHook` traits from `aura-agent-kernel` to a new exec-layer home — tracked as a Phase 10 follow-up in [tests/layer_boundary.rs](../tests/layer_boundary.rs).

#### [`aura-exec-conflict`](../crates/aura-exec-conflict)

Domain-scoped advisory locks (`ConflictRegistry`, `ConflictDomain`, `LockHandle`, `ConflictError`) that reduce sibling collisions when multiple agents target the same logical resource. Isolation (worktree / copy) is the hard safety guarantee; conflict locks are an optimisation on top.

#### [`aura-exec-isolation`](../crates/aura-exec-isolation)

Subagent workspace isolation. `WorktreeIsolation` (git worktree) is the preferred path; `CopyIsolation` is the fallback when git is unavailable or the workspace is not a git repo. Returns an `IsolatedWorkspace` handle that the spawner mounts before scheduling the child.

#### [`aura-exec-policy`](../crates/aura-exec-policy)

Pure approval / verdict evaluation over already-resolved effective permissions for a tool call. `evaluate`, `ToolApproval`, `PolicyError`. Has no `ModelProvider` or `Store` deps — it's a small pure-function crate the kernel orchestrates.

#### [`aura-exec-sandbox`](../crates/aura-exec-sandbox)

OS-level containment primitives: `FsSandbox` (path canonicalisation, prefix-check, symlink guard) and `ProcessSandbox` (subprocess spawn guardrails). Used by every filesystem and command tool.

#### [`aura-exec-tools`](../crates/aura-exec-tools)

Layered re-export shell over `aura-tools` plus `sandbox` and `policy` sub-modules so exec consumers get one import surface.

#### [`aura-exec-runner`](../crates/aura-exec-runner)

Layered alias for `ToolExecutor` with `conflict::ConflictRegistry` and `isolation::WorktreeIsolation` re-exports. Future home for the orchestration logic that today lives in `aura-tools::executor`.

#### [`aura-tools`](../crates/aura-tools)

Tool catalog, resolver, and sandboxed filesystem/command execution. Implements the `Executor` trait from `aura-agent-kernel`. Hosts:

- Built-in filesystem tools: `list_files`, `read_file`, `write_file`, `edit_file`, `stat_file`, `find_files`, `delete_file`, `search_code` (ripgrep), `run_command` (with sync / async threshold).
- Git tools under `git_tool/`: `git_commit`, `git_push`, `git_commit_push` — the *only* permitted call-site for mutating `Command::new("git")` (Invariant §1, enforced by `scripts/check_invariants.sh`).
- Domain tools under `domain_tools/`: HTTP/API-backed handlers for orbit, network, specs, tasks, projects, storage via the `DomainApi` trait. The HTTP `DomainApi` implementation itself lives in the surface-layer [`aura-domain-http`](#aura-domain-http) crate — `aura-tools` only owns the trait + the tool wrappers.
- `task` tool — consumes the exec-layer `SubagentDispatchHook` trait this crate declares. The trait was always here; Phase B kept it put while moving the concrete dispatcher to the new fleet-layer `aura-fleet-subagent` crate so the strict layer boundary holds.
- Automaton tools under `automaton_tools/`: dev-loop and task-run controls gated behind an `AutomatonController` trait.
- `permissions.rs` (new in Phase B / Commit 3) — `load_agent_tool_context` + `validate_user_defaults` + `validate_agent_tool_permissions`, lifted from the pre-refactor top-level `aura-runtime/src/tool_permissions.rs`. Pure utility functions consumed by the gateway's HTTP handlers; the gateway keeps the routing/HTTP wrapper alongside its other handlers.
- Catalog + resolver: `ToolCatalog`, `ToolResolver`, `ToolProfile` (`Core` / `Agent` / `Engine`), `CatalogEntry`. The resolver's trusted-integration helpers (GitHub, Linear, Slack, Resend, Brave) live under `resolver/trusted/integrations/`.

---

### Agent layer

The deterministic core of a single agent. The kernel records every reasoning call, every tool proposal, every policy decision, every effect. **Invariants §1 through §11** are all owned here. The legacy `aura-kernel` crate is a re-export shell over the renamed `aura-agent-kernel`.

#### [`aura-agent-kernel`](../crates/aura-agent-kernel)

The deterministic kernel. Builds context from the record window, calls the reasoner, enforces policy, dispatches execution through the `ExecutorRouter`, and produces `RecordEntry`s. Given the same record, produces the same output.

Key types: `Kernel` (with `process_direct`, `process_dequeued`, `reason`, `reason_streaming`, `process_tools`), `KernelConfig` (`record_window_size`, `policy`, `workspace_base`, `replay_from`, `proposal_timeout_ms`), `ExecutorRouter`, `Executor`, `ExecuteContext`, `Policy`, `PolicyConfig`, `ContextBuilder`, `ReplayConsumer`. The `kernel/tools/` proposal pipeline is split into `{mod,single,batch,shared}.rs`; the `policy/check/` module is split into per-gate helpers.

#### [`aura-agent-loop`](../crates/aura-agent-loop)

Thin re-export shell over `aura-agent`'s multi-step turn loop. Provides `AgentLoop`, `AgentLoopConfig`, `TurnEvent`, `RunOptions` at a stable surface ahead of a future extraction of the loop body into its own crate.

#### [`aura-agent-steering`](../crates/aura-agent-steering)

Stateful per-turn steering evaluators. Built-ins: `RepeatedReadTracker`, `ImplementNow`, `EarlyOracle`. `SteeringRegistry` and `TurnSteering` thread them through each iteration; `inject` renders steering hints into the active system prompt.

#### [`aura-agent-subagent`](../crates/aura-agent-subagent)

Subagent derivation, inheritance, registry, and the pure-data adapter layer that any agent-layer caller needs to feed a child spawn. `derive_subagent(parent, request)` produces a `SubagentSpec` that may only narrow the parent's mode, permissions, and model. Owns:

- `derivation.rs` — `derive_subagent` and the `SubagentDerivation` trait.
- `parent.rs` — `ParentContext` (the atomically-captured parent session snapshot).
- `overrides.rs` — `SubagentOverrides`, `SubagentBudget`.
- `manifest.rs` — `OverrideManifest`, `OverriddenField`.
- `spec.rs` — `SubagentSpec`, `AuditAttribution`, `SubagentLineage`.
- `errors.rs` — the `DerivationError` closed taxonomy.
- `registry.rs` — `SubagentRegistry` + bundled kinds, absorbed from the pre-Phase-B `aura-runtime/src/subagent_registry.rs`.
- `adapters.rs` — `parent_context_from_request`, `overrides_from_request`, `narrow_permissions`, `legacy_permissions_to_modes`, the `core_to_modes_*` translators. These are the dispatcher's pure-data helpers, lifted out of the old `aura-runtime/src/subagent_dispatch.rs` so the agent layer owns every piece of subagent state assembly without taking any fleet deps.

The `SubagentDispatchHook` trait stays at the **exec layer** in `aura-tools` (unchanged — see below); the fleet-layer concrete impl lives in `aura-fleet-subagent`. Phase 7a routed the `task` tool through this crate; Phase B widened it to also own the dispatcher's data layer.

#### [`aura-agent`](../crates/aura-agent)

The multi-step orchestration loop and everything that wraps the kernel: streaming, blocking detection, stall detection, budget management, compaction orchestration, build verification, message sanitization, planning, self-review, file-ops pipeline.

Key types and modules:

- `AgentLoop` + `AgentLoopConfig` + `LoopState` — the loop itself (`agent_loop/{mod,iteration,streaming,tool_execution,tool_pipeline,tool_result_cache,context,search_cache,state,steering}.rs`).
- `KernelToolGateway`, `KernelModelGateway`, `KernelDomainGateway` — kernel bridges that implement the traits the loop and automatons expect (Invariant §8). `KernelDomainGateway` is the sole `DomainApi` wrapper.
- `RecordingModelProvider` — sealed marker trait. The only type that satisfies it from outside `aura-agent` is `KernelModelGateway` (Phase 4 type-level §1 seal).
- `agent_runner/` — higher-level run coordination for autonomous task execution.
- `session_bootstrap.rs` — shared embedder bootstrap (`open_store`, `default_agent_config`, `build_executor_router`).
- `blocking/`, `stall.rs`, `budget.rs`, `build.rs`, `verify/`, `planning.rs`, `self_review.rs`, `sanitize.rs`, `read_guard.rs`, `file_ops/`.
- `git.rs` — **read-only** git helpers only (`is_git_repo`, `list_unpushed_commits`). Mutating ops moved to `aura-tools/src/git_tool/`.
- `events/` — `TurnEventSink`, the agent-loop event enum, and the `map_agent_loop_event` dispatch shared by the TUI `UiCommandSink` and the runtime `OutboundMessageSink`.

#### [`aura-kernel`](../crates/aura-kernel) (shell)

Re-export shell over `aura-agent-kernel` preserving historical `aura_kernel::*` paths.

---

### Fleet layer

The multi-agent runtime. Above the single-agent kernel: registry of live agents, spawn pipeline, dispatch, quota tracking, mailbox, the concrete subagent dispatcher, and the composition root that wires them together. **Invariant §12** (single writer per agent) crosses the agent/fleet boundary — after Phase B the scheduler's per-agent processing claim lives in `aura-engine` (surface) but its semantics are defined here.

#### [`aura-fleet-registry`](../crates/aura-fleet-registry)

In-memory directory of live and recently-terminated agents known to the fleet daemon. `FleetRegistry`, `AgentSlot`, `AgentState`, `RegistryError`. Read-mostly via `RwLock<HashMap<AgentId, AgentSlot>>`.

#### [`aura-fleet-quota`](../crates/aura-fleet-quota)

Concurrency and resource budgets across the fleet. `QuotaPool` plus the RAII `BudgetTicket` that releases its slot on drop. Phase 7b promoted this from tracking-only to enforcing.

#### [`aura-fleet-spawn`](../crates/aura-fleet-spawn)

The subagent spawn pipeline: idempotent dedupe, per-parent audit-append lease (`ParentLeaseRegistry`), `derive_subagent` invocation, quota ticket acquisition, `RecordEntry` audit append, and the `SpawnMode::{Wait, Detached, Batch}` execution. Orphan handoff on parent death is owned by the `OrphanStore` here.

Key types: `FleetSpawner`, `SpawnHandle`, `ParentLeaseRegistry`, `OrphanStore`. Replaced the legacy `spawn_lock` mechanism.

#### [`aura-fleet-dispatch`](../crates/aura-fleet-dispatch)

Routes a stream of `AgentJob` items into `FleetSpawner::spawn`. Does not own enqueue or persistence — those live in `aura-fleet-mailbox` and `aura-store-db` respectively.

#### [`aura-fleet-mailbox`](../crates/aura-fleet-mailbox)

Bounded MPSC mailbox of `AgentJob` with backpressure and typed send errors. `Mailbox`, `MailboxSender`, `MailboxReceiver`, `MailboxError`.

#### [`aura-fleet-daemon`](../crates/aura-fleet-daemon)

Composition root that wires registry, spawner, dispatcher, quota, and mailbox into a single `FleetDaemon` handle. Also hosts `resolve_session_mode` and `AgentModeInputs` — the helpers that implement the documented `AgentMode` resolution chain.

#### [`aura-fleet-subagent`](../crates/aura-fleet-subagent)

Phase B / Commit 3 crate hosting the concrete `FleetSubagentDispatcher` impl of `aura_tools::SubagentDispatchHook`. Composes `aura-fleet-spawn::FleetSpawner` + `aura-fleet-registry` + `aura-fleet-quota` with `aura-agent-subagent::SubagentRegistry` and a surface-layer `Arc<dyn ChildRunner>` (provided by `aura-engine`'s `RuntimeChildRunner`). The dispatcher applies the agent-layer derivation chain (`derive_subagent` → quota ticket → audit append → child runner), is fail-closed (errors surface through `SubagentResult`), and is the only place the fleet wires `SpawnMode::{Wait, Detached, Batch}` outcomes into the task tool's response.

The split exists for a strict layer-boundary reason: this crate sits at the fleet layer where the spawner/quota/registry deps live, so any agent context (chat, dev-loop, task-run, a future embedded SDK call) gets the dispatcher by depending on this one crate — not by reaching up to the surface-layer composition root. All Cargo edges remain downward; no new `WARN_ONLY_UPWARD_EDGES` entries.

---

### Surface layer

Composition roots. Each surface assembles dependencies from the lower layers into a runnable entry point (CLI, TUI, SDK, headless server, automaton host) or a side-effectful client (zOS HTTP). Phase 9 introduced the dedicated `aura-surface-*` shells; the older `aura-runtime`, `aura-terminal`, `aura-automaton`, `aura-auth` crates remain at this layer and are referenced through the shells where applicable.

#### [`aura-surface-cli`](../crates/aura-surface-cli)

CLI composition root. Owns the clap `Cli` / `Commands` / `RunArgs` definitions, the `ModeFlag` global flag (top of the `AgentMode` resolution chain), the event-loop wiring (`event_loop/`), the record-loader utility, and the surface-layer `session_helpers` that chain `aura_auth::CredentialStore` onto `aura_agent::session_bootstrap`. The workspace root `src/main.rs` is now a thin binary that delegates to `aura_surface_cli::run`; the `aura-node` binary still enters through `aura_runtime::run_node` and is re-exported as `aura_surface_cli::run_node` to avoid a dependency cycle.

#### [`aura-surface-sdk`](../crates/aura-surface-sdk)

External SDK types for talking to a fleet daemon over `aura-core-protocol`. `AuraClient`, `AuraSession`, `SessionConfig` (which carries the documented `mode: Option<AgentMode>` field that feeds the resolution chain), `SdkError`. Transport is pluggable — the SDK itself is type-shape only.

#### [`aura-surface-terminal`](../crates/aura-surface-terminal)

Phase 9 shell over the legacy `aura-terminal` crate. Adds the typed `SlashModeCommand` for parsing `/mode <agent|plan|ask|debug>` from the TUI input layer.

#### [`aura-surface-automaton`](../crates/aura-surface-automaton)

Phase 9 shell over the legacy `aura-automaton` crate.

#### [`aura-surface-auth`](../crates/aura-surface-auth)

Phase 9 shell for zOS HTTP client and credential storage (`ZosClient`, `CredentialStore`, `StoredSession`). The token primitive types stay at the `core` layer in `aura-core-auth` and are re-exported here.

#### [`aura-runtime`](../crates/aura-runtime)

The HTTP/WS **gateway crate** plus the composition root for the `aura-node` binary. After the Phase C / Commit 4 reorganization `aura-runtime` no longer owns scheduling, orchestration, subagent dispatch, HTTP domain calls, or the PTY terminal handler — those moved out to dedicated crates (see the next two entries). What remains is the inbound-traffic seam plus the composition root that assembles every lower layer into a runnable server.

`aura-runtime` is the **sole Cargo surface** for any external Rust consumer of the harness (see the [External-consumer invariant](#external-consumer-invariant) callout at the top of this document). External consumers that need in-process types must depend on this crate and read them through its re-exports — never reach for `aura-engine`, `aura-domain-http`, `aura-agent-subagent`, or any other lower crate directly.

Layout:

- `lib.rs` — re-exports + `run_node()` entry point (calls `Node::run`). Phase 10 carve-out 1 owns the `aura-node` binary body here so `crates/aura-runtime/src/main.rs` is a ≤10-line shim.
- `main.rs` — `tokio::main` shim that calls `run_node()`.
- `node.rs` — `Node` composition root: binds the TCP listener, opens the store, builds the `aura-engine::Engine`, builds the `aura-fleet-subagent::FleetSubagentDispatcher` over the engine's `ChildRunner`, builds the `aura-domain-http::HttpDomainApi`, builds the gateway `Router`, and runs the axum server with graceful shutdown.
- `config/` — `NodeConfig::from_env`: env loading, paths, auth-token resolution. Module path unchanged across the refactor.
- `auth.rs` — bearer-token check shared with `aura-surface-cli`'s embedded API server.
- `inbound_console.rs` — paired request/failure observer formatting under the `aura::console` target.
- `files_api.rs` — shared workspace walker and capped file reader used by the node's `/v1/files` + `/v1/read-file` and the TUI-embedded `src/api_server.rs`.
- `tool_permissions.rs` — HTTP-driven tri-state tool permission writes. The single sanctioned non-kernel `append_entry_*` call site, guarded by the per-agent scheduler claim.
- `protocol.rs` — wire ↔ core conversion shim now reduced to a re-export adapter; the canonical conversions live in `aura-protocol::conversions`.
- `gateway/` — the HTTP/WS gateway, renamed from `router/` in Phase C:
  - `mod.rs` — module declarations + `pub use create_router / RouterState / RouterStateConfig`.
  - `middleware.rs` — `create_router`, the middleware stack (CORS, governor, body-limit, connect-info, observer, timeout, trace), the terminal-WS delegate (`terminal_ws_handler` → `aura_terminal::ws::handle_terminal_ws`), and the `/health` handler.
  - `state.rs` — `RouterState` + `RouterStateConfig` threaded through every handler.
  - `auth_mw.rs` — bearer-token enforcing axum middleware.
  - `errors.rs` — `ApiError` JSON failure shape.
  - `handlers/` — per-endpoint bundles: `run.rs` (`POST /v1/run` + the lifecycle endpoints), `run_ws.rs` (`WS /stream/:run_id`), `files.rs`, `tx.rs`, `memory/`, `skills.rs`, `tool_permissions.rs`, `util.rs`.
  - `session/` — per-WebSocket-connection state. `chat.rs` (bidirectional Chat-run handler), `cross_agent_hook.rs` (cross-agent callback path — the one place `aura-runtime` still calls `reqwest`), `generation.rs` (generation proxy guarded by [`tests/generation_proxy_guard.rs`](../crates/aura-runtime/tests/generation_proxy_guard.rs)), `helpers.rs`, `partial_json.rs`, `state.rs`, `tool_approval.rs`.

Backward-compat re-exports preserved at the crate root: `aura_runtime::scheduler::*` and `aura_runtime::memory_observer::*` now resolve to thin re-export modules over `aura_engine::*`. Internal callers should reach `aura-engine` directly, but the old paths keep `aura-surface-cli` and any future re-export-driven external consumer building without ripple changes.

HTTP routes are listed in [README.md#http--websocket-api](../README.md#http--websocket-api).

#### [`aura-engine`](../crates/aura-engine)

Phase B / Commit 3 crate that owns the orchestration engine. Lives at the surface layer because every member composes lower-layer (kernel + store + fleet + agent) types into a runnable orchestration loop; the engine is what `aura-runtime`'s gateway hands `RuntimeRequest` payloads to. Splitting it out of `aura-runtime` lets the gateway crate shrink to inbound-only concerns, and lets a future surface composition (e.g. an in-process SDK host) re-use the engine without dragging in HTTP/WS plumbing.

Layout:

- `scheduler.rs` — per-agent single-writer claim (Invariant §12.a), `IdentityRegistry`, per-turn `AgentLoopConfig` resolution. The claim semantics are unchanged from the pre-refactor `aura-runtime/src/scheduler.rs` — only the crate moved.
- `worker.rs` — `process_agent`: dequeue + `AgentLoop` execution + atomic record append. Owns the `AGENT_LOOP_TIMEOUT` boundary timeout (rules.md §6.2).
- `automaton/{mod,build,dispatch,event_channel}.rs` — `AutomatonBridge`: builds per-agent automaton kernels, turns automaton events into outbound `OutboundMessage` frames, records lifecycle changes. Formerly `aura-runtime/src/automaton_bridge/`.
- `memory_observer.rs` — `MemoryTurnObserver`: `TurnObserver` adapter feeding completed turns into `aura_memory::MemoryManager`.
- `capabilities.rs` — `record_runtime_capabilities` helper called by the automaton bridge during dev-loop / task-run bootstrap.
- `executor.rs` — shared `ToolResolver` / `ExecutorRouter` construction helpers (formerly `executor_factory.rs`).
- `child_runner.rs` — `RuntimeChildRunner`: the surface-layer impl of `aura_fleet_spawn::ChildRunner`. Exposed as `Engine::child_runner() -> Arc<dyn ChildRunner>`; consumed by the fleet-layer `FleetSubagentDispatcher`.
- `model_context.rs` — `context_window_for_model` lookup used by the automaton bridge and the gateway-side session config.
- `lib.rs` — re-exports `AutomatonBridge`, `EventSubscription`, `RuntimeChildRunner`, `MemoryTurnObserver`, `turn_summary_from_result`, `context_window_for_model`, `Scheduler`, `SchedulerError`, `AgentIdentity`, `AgentIdentityRegistry`, `process_agent_detailed`, `ProcessedAgent`.

Library crate, so it ships typed `thiserror` errors (rules.md §4.2) — `aura-runtime`'s binary `aura-node` still uses `anyhow` at the top-level `main`.

#### [`aura-domain-http`](../crates/aura-domain-http)

Phase C / Commit 4 crate hosting the HTTP `DomainApi` implementation that the kernel domain gateway and automaton bridge consume. Stays at the surface layer because it composes lower-layer storage / network clients into a deployable HTTP edge — analogous to how `aura-runtime` composes the engine + middleware stack into a deployable HTTP server.

Layout:

- `http.rs` — `HttpDomainApi`: `reqwest`-backed `DomainApi` impl with Cloudflare-block retry handling, `aura-os-server` base-URL override routing, and JWT-bearer authentication.
- `jwt.rs` — `JwtDomainApi`: wraps any other `DomainApi` and injects a captured JWT on every call site that didn't supply one. Used by the automaton bridge to stamp the captured bearer onto every domain call without modifying the underlying `DomainApi` impl.
- `lib.rs` — re-exports `HttpDomainApi` and `JwtDomainApi`.

The `DomainApi` *trait* continues to live in `aura-tools::domain_tools`; only the HTTP implementations live here. `aura-runtime` therefore no longer depends on `reqwest` *for domain calls* — but it still depends on `reqwest` directly for the cross-agent callback path in `gateway/session/cross_agent_hook.rs`, which is a separate outbound surface.

#### [`aura-terminal`](../crates/aura-terminal)

Ratatui-based terminal UI library: themed rendering, components (`HeaderBar`, `InputField`, `Message`, `ToolCard`, `StatusBar`, `ProgressBar`, `DiffView`), input handling, layout. `App` is the UI state machine; `UiEvent` and `UiCommand` are the bridge to the agent loop.

#### [`aura-automaton`](../crates/aura-automaton)

Long-running automaton workflows that drive `AgentLoop` on a schedule. Built-ins: `ChatAutomaton`, `DevLoopAutomaton` (with commit-and-push support; `dev_loop/{mod,aggregate,forward_event,validation}.rs`), `SpecGenAutomaton`, `TaskRunAutomaton`. `AutomatonRuntime` installs / runs / cancels instances; `TickContext` carries per-tick state.

#### [`aura-auth`](../crates/aura-auth)

zOS login client (`ZosClient`) and credential persistence (`CredentialStore` over `~/.aura/credentials.json` + OS keyring). Re-exports the pure token types from `aura-core-auth`.

---

## Part 2 — User flows

The same kernel/AgentLoop pipeline drives every front-end. Before any external effect, the resolved `AgentMode` (CLI flag > TUI slash > SDK field > daemon default > `AgentMode::Agent` fallback) gates the action; the policy layer then narrows further per-tool.

### Flow 1: Interactive TUI session

Default mode when a user runs `cargo run` or `aura`. Participants (left → right): **User**, **TUI** (`aura-terminal`), **EL** (event loop in `aura-surface-cli`), **AL** (`AgentLoop` in `aura-agent`), **MP** (`ModelProvider` in `aura-reasoner`), **KTG** (`KernelToolGateway` + `ExecutorRouter` in `aura-agent-kernel`), **Tools** (`ToolExecutor` in `aura-tools`).

```text
 User       TUI         EL            AL              MP           KTG         Tools
  │          │           │             │               │             │           │
  │ types    │           │             │               │             │           │
  │ Enter ──▶│           │             │               │             │           │
  │          │ UiEvent:: │             │               │             │           │
  │          │ UserMsg ─▶│             │               │             │           │
  │          │           │ append +    │               │             │           │
  │          │           │ run_with_   │               │             │           │
  │          │           │ events ────▶│               │             │           │
  │          │           │             │ compact ctx   │             │           │
  │          │           │             │ build req     │             │           │
  │          │           │             │ complete_     │             │           │
  │          │           │             │ streaming ───▶│             │           │
  │          │           │             │◀── Stream of StreamEvents ──│           │
  │          │           │◀── TurnEvent::TextDelta /                 │           │
  │          │           │   ThinkingDelta / ToolStart                           │
  │          │◀── UiCommand::AppendText / StartThinking / ShowTool               │
  │ renders  │           │             │               │             │           │
  │ text + ◀─│           │             │               │             │           │
  │ cards    │           │             │               │             │           │
  │          │           │             │               │             │           │
  │          │  ┌─ Stop reason ────────┴──── ToolUse ──┴────┐        │           │
  │          │  │                                            ▼        │           │
  │          │  │             │             │ executor.execute(uncached) ──────▶│
  │          │  │             │             │               │  mode+policy check│
  │          │  │             │             │               │  router.execute ─▶│
  │          │  │             │             │               │             sandboxed
  │          │  │             │             │               │             FS / cmd
  │          │  │             │             │               │◀── Effect ─┤
  │          │  │             │             │◀── Vec<ToolCallResult> ────│
  │          │  │             │◀── TurnEvent::ToolResult (per tool)       │
  │          │◀── UiCommand::CompleteTool ──┤                              │
  │          │  │                                                          │
  │          │  └─ Stop reason ─── EndTurn ─┐                              │
  │          │                              ▼                              │
  │          │◀── UiCommand::Complete ──── TurnEvent::StepComplete ────────┤
  │ final ◀──│                              │                              │
  │          │                              │ (next iteration or exit)     │
```

**Data path:** User input → `UiEvent` channel → Event Loop appends to `Vec<Message>` → `AgentLoop.run_with_events()` → streaming `TurnEvent`s back through `mpsc` channel → Event Loop maps to `UiCommand` → Terminal renders.

---

### Flow 2: Run kickoff + WebSocket session (`aura-runtime` + `aura-engine`)

Used by `aura-os` and other clients connecting over the harness wire. After Phase A this is a **two-step exchange**: the client submits a `RuntimeRequest` over HTTP, receives a `run_id` back, then opens a per-run WebSocket. The client never sends an init frame — the run is already up by the time the WS attaches. Participants: **Client**, **GW** (gateway HTTP/WS handlers in `aura-runtime`), **Engine** (`aura-engine::Engine`), **Sess** (per-WS session state), **AL** (`AgentLoop`), **MP** (`ModelProvider`), **KTG** (`KernelToolGateway`), **Tools** (`ToolExecutor`).

```text
 Client            GW              Engine           Sess          AL         MP / KTG / Tools
   │                │                │                │             │              │
   │ POST /v1/run                    │                │             │              │
   │ { type: Chat | DevLoop | TaskRun,                │             │              │
   │   agent_identity, model,        │                │             │              │
   │   workspace, project,           │                │             │              │
   │   agent_permissions,            │                │             │              │
   │   tool_permissions,             │                │             │              │
   │   agent_capabilities,           │                │             │              │
   │   auth_jwt, user_id }           │                │             │              │
   │───────────────▶│                │                │             │              │
   │                │ resolve_session_mode(inputs) → AgentMode      │              │
   │                │ Engine::submit(req) ───────────▶│             │              │
   │                │                │ build per-run state          │              │
   │                │                │ register pending_chat_runs   │              │
   │                │◀── RunHandle { run_id, event_stream_url } ────│              │
   │◀── 201 Created                  │                │             │              │
   │   { run_id,                     │                │             │              │
   │     event_stream_url:           │                │             │              │
   │       "/stream/:run_id" }       │                │             │              │
   │                │                │                │             │              │
   │ WS connect /stream/:run_id      │                │             │              │
   │───────────────▶│                │                │             │              │
   │                │ open_chat_stream(run_id) ──────▶│             │              │
   │                │ create Session ────────────────▶│             │              │
   │◀── Outbound::SessionReady ──────│                │             │              │
   │                │                │                │             │              │
   │ Inbound::UserMessage ──────────▶│                │             │              │
   │                │ append user msg ──────────────▶│             │              │
   │                │ run_with_events ──────────────────────────────▶│             │
   │                │                │                │             │ complete_streaming ──▶ MP
   │                │                │                │             │◀── StreamEvents ──────
   │                │◀── TurnEvent::TextDelta ─────────────────────│              │
   │◀── Outbound::TextDelta { text }                                              │
   │                │                                                              │
   │                │ ── alt: tool execution ─────────────────────────────────┐   │
   │                │                │                │             │ KTG.execute ──▶ Tools
   │                │                │                │             │           sandbox ──▶
   │                │                │                │             │◀── results ────────
   │                │◀── TurnEvent::ToolResult ───────────────────│              │
   │◀── Outbound::ToolResult { name, result, is_error }                           │
   │                │ ── end alt ────────────────────────────────────────────────┘ │
   │                │                                                              │
   │                │◀── AgentLoopResult ────────────────────────│                │
   │◀── Outbound::AssistantMessageEnd { usage, files_changed }                    │
   │                                                                                │
   │ Inbound::Cancel ─────────────▶│ CancellationToken::cancel() ─▶│              │
```

**Data path:** Client `POST /v1/run` (body: `RuntimeRequest`) → gateway resolves the auth-token chain, resolves `AgentMode` via `aura_fleet_daemon::resolve_session_mode`, calls `Engine::submit(req)` which prepares the per-run state and (on `Chat`) parks the pending session in `RouterState::pending_chat_runs` keyed by the new `run_id` → gateway returns `{ run_id, event_stream_url }` synchronously. Client follows up with `WS /stream/:run_id`; the gateway pulls the parked session, emits `OutboundMessage::SessionReady` as the first server frame (no client init frame at all), and bridges subsequent `TurnEvent`s to `OutboundMessage` for the duration of the run. A `Cancel` may arrive at any time and trips the `CancellationToken`.

`DevLoop` and `TaskRun` follow the same two-step exchange — the synchronous `POST /v1/run` returns a `run_id`, and the WS at `/stream/:run_id` is event-only (no `user_message` frames inbound). The gateway handler routes the request to `AutomatonBridge::start_dev_loop` / `AutomatonBridge::start_task_run` in `aura-engine` before responding.

---

### Flow 3: Headless node (scheduler-driven)

When running `aura run --ui none` or as `aura-node`, transactions are submitted via HTTP and processed by the engine's scheduler. Subagent spawns from inside the loop go through `aura-fleet-subagent::FleetSubagentDispatcher`, which composes `aura-fleet-spawn` with the engine-layer `RuntimeChildRunner`. Participants: **Client**, **Router** (HTTP gateway in `aura-runtime`), **Store** (`RocksStore`), **Sched** (`aura-engine::Scheduler`), **Worker** (`aura-engine::process_agent`), **AL** (`AgentLoop`), **MP** (`ModelProvider`), **KTG** (`KernelToolGateway`), **Fleet** (`FleetSubagentDispatcher` + `FleetSpawner`).

```text
 Client     Router       Store        Sched         Worker         AL          MP / KTG / Fleet
   │          │            │            │              │            │                 │
   │ POST /tx │            │            │              │            │                 │
   │─────────▶│ enqueue_tx │            │              │            │                 │
   │          │───────────▶│            │              │            │                 │
   │◀── 202 ──│            │            │              │            │                 │
   │          │            │            │              │            │                 │
   │          │            │◀── check pending txs ─────│            │                 │
   │          │            │            │              │            │                 │
   │          │            │            │ acquire per-agent claim   │                 │
   │          │            │            │ (Invariant §12)           │                 │
   │          │            │            │ process_agent ───────────▶│                 │
   │          │            │◀── dequeue_tx ────────────│            │                 │
   │          │            │── (token, Transaction) ──▶│            │                 │
   │          │            │            │              │ agent_loop.run ────────────▶│
   │          │            │            │              │            │                 │
   │          │            │            │              │            │ ── loop ───────┐│
   │          │            │            │              │            │ model call ───▶│ MP
   │          │            │            │              │            │◀── response ───│
   │          │            │            │              │            │                 │
   │          │            │            │              │            │ alt: tool exec  │
   │          │            │            │              │            │ KTG ───────────▶│ Tools
   │          │            │            │              │            │◀── results ────│
   │          │            │            │              │            │                 │
   │          │            │            │              │            │ alt: task tool  │
   │          │            │            │              │            │ Fleet (derive_  │
   │          │            │            │              │            │ subagent +      │
   │          │            │            │              │            │ spawn) ────────▶│ Fleet
   │          │            │            │◀── schedule_agent_with_overrides(child) ────│
   │          │            │            │── child handle (Wait/Detached/Batch) ───────│
   │          │            │            │              │            │ ── end loop ───┘│
   │          │            │            │              │◀── AgentLoopResult ──────────│
   │          │            │◀── append_entry_atomic(agent_id, seq, entry, token) ─────│
   │          │            │            │◀── Worker done ───────────│                 │
   │          │            │            │                                              │
   │ GET /agents/{id}/record?from=1&limit=10                                          │
   │─────────▶│ scan_record│            │                                              │
   │          │───────────▶│            │                                              │
   │          │◀── Vec<RecordEntry>     │                                              │
   │◀── JSON record entries                                                            │
```

**Data path:** HTTP POST → Store inbox → Scheduler dequeues → Worker runs `AgentLoop` → Result committed atomically to record log → Client polls via GET. Subagent spawn lands through `aura-fleet-subagent::FleetSubagentDispatcher` (which composes `aura-fleet-spawn` with the engine-layer `RuntimeChildRunner`) and re-enters the same scheduler lane, inheriting the per-agent processing claim (Invariant §12.a).

---

### Flow 4: Streaming error recovery (`StreamReset`)

When a streaming model call fails mid-stream, the system recovers deterministically. Participants: **AL** (`AgentLoop`), **MP** (`ModelProvider`), **UI** (TUI or WS consumer).

```text
 AL                              MP                             UI
  │                               │                              │
  │ provider.complete_streaming ─▶│                              │
  │◀── StreamEvent::TextDelta("partial...")                      │
  │ TurnEvent::TextDelta("partial...") ────────────────────────▶│
  │                               │                              │ renders
  │                               │                              │ partial text
  │                               │                              │
  │◀── StreamEvent::Error (connection lost)                      │
  │ TurnEvent::StreamReset { reason: "Stream error, retrying" } ▶│
  │                               │                              │ clears buffered
  │                               │                              │ partial content
  │                               │                              │
  │ provider.complete(request) ──▶│  (non-streaming fallback)    │
  │◀── Complete ModelResponse     │                              │
  │ TurnEvent::TextDelta(full_text) ───────────────────────────▶│
  │                               │                              │ renders
  │                               │                              │ authoritative
  │                               │                              │ content
```

---

### Data lifecycle summary

```text
 INPUT                 PROCESSING                                      OUTPUT
 ─────                 ──────────                                      ──────

 User Prompt           ┌─────────────────────────────────────────┐
     │                 │                                         │
     ▼                 │  AgentLoop                              │
 Transaction ─────────▶│     │                                   │
                       │     ▼                                   │
                       │  ModelProvider ──▶ ModelResponse        │
                       │                       │                 │
                       │                  StopReason?            │
                       │                 ╱          ╲            │
                       │           ToolUse          EndTurn      │     Record Entry
                       │              │                │         │──▶  ──▶ RocksDB
                       │              ▼                ▼         │
                       │  KernelToolGateway      AgentLoopResult │──▶  TurnEvents
                       │       │                                 │     ──▶ UI / WS
                       │       ▼                                 │
                       │  ExecutorRouter                         │
                       │       │                                 │
                       │       ▼                                 │
                       │  ToolExecutor + Sandbox                 │
                       │       │                                 │
                       │       ▼                                 │
                       │    Effect ───────▶ (back to AgentLoop)  │
                       │                                         │
                       └─────────────────────────────────────────┘
```

Every user interaction follows the same fundamental path: input becomes a transaction, the `AgentLoop` orchestrates model calls and tool execution in a loop, results are emitted as `TurnEvent`s for real-time display, and the final state is persisted as a `RecordEntry` in the append-only log.

For invariants enforcement details and the full per-invariant CI gate map, see [docs/invariants.md](invariants.md). For the running record of what landed in each phase of the refactor, see [CHANGELOG.md](../CHANGELOG.md).
