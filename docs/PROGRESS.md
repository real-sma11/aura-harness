# Aura Harness - Implementation Progress

## Overview

This file is the historical implementation log for the Aura Harness workspace. The current source of truth for crate boundaries and wire flow is:

- [`README.md`](../README.md) — operator-facing build, API, and development reference.
- [`docs/architecture.md`](architecture.md) — layered crate map, external-consumer invariant, and run/WebSocket flows.

The active post-Phase-A/B/C shape is a self-contained Cargo workspace with `aura-runtime` as the sole external Rust surface and HTTP/WS gateway. `aura-engine` owns orchestration, `aura-domain-http` owns the HTTP `DomainApi` implementation, and `aura-fleet-subagent` owns the concrete fleet subagent dispatcher. Clients start work with `POST /v1/run` (`RuntimeRequest` → `RuntimeRunResponse`) and then attach to `WS /stream/:run_id`; there is no client-sent `SessionInit` first frame.

Original MVP specifications:
- `specs/spec-01.md` - MVP Foundation (Complete)
- `specs/spec-02.md` - Interactive Coding Runtime (In Progress)

**Start Date:** 2026-01-08
**Last Updated:** 2026-05-28

---

## Build Requirements

### Windows
RocksDB requires LLVM/Clang to build. Install via:
```powershell
winget install LLVM.LLVM
# Set environment variable:
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
```

### All Platforms
- Rust 1.80+ MSRV (via rustup); the checked-in `rust-toolchain.toml` uses stable for local builds.
- rustfmt: `rustup component add rustfmt`
- clippy: `rustup component add clippy`

---

## Implementation Phases

### Phase 1: Core Foundation (`aura-core`) 
**Status:** 🟢 Complete

Core types, IDs, serialization, and error handling.

- [x] Workspace + Cargo.toml setup
- [x] `AgentId` newtype (`[u8; 32]`)
- [x] `TxId` newtype (`[u8; 32]`)
- [x] `ActionId` newtype (`[u8; 16]`)
- [x] `Transaction` struct + `TransactionKind` enum
- [x] `Action` struct + `ActionKind` enum
- [x] `Effect` struct + `EffectKind` + `EffectStatus` enums
- [x] `Proposal` + `ProposalSet` structs
- [x] `Decision` struct
- [x] `RecordEntry` struct
- [x] `Identity` struct
- [x] `ToolCall` + `ToolResult` structs
- [x] Error types with `thiserror`
- [x] Serde serialization (JSON)
- [x] Hashing utilities (blake3)
- [x] Unit tests for serialization round-trips

---

### Phase 2: Storage Layer (`aura-store`)
**Status:** 🟢 Complete (code written, requires LLVM to build)

RocksDB implementation with column families and atomic commits.

- [x] RocksDB dependency setup
- [x] Column family definitions (record, agent_meta, inbox)
- [x] Key encoding/decoding utilities
- [x] `Store` trait definition
- [x] `RocksStore` implementation
- [x] `enqueue_tx` - durable inbox write
- [x] `dequeue_tx` - peek + return inbox item
- [x] `get_head_seq` - read agent head
- [x] `append_entry_atomic` - WriteBatch commit
- [x] `scan_record` - range scan for record window
- [x] Agent metadata operations
- [x] Unit tests for atomicity
- [x] Unit tests for key ordering

---

### Phase 3: Executor Framework (now `aura-exec-*` + `aura-agent-kernel`)
**Status:** 🟢 Complete

Executor trait and router for dispatching actions. The original milestone name predated the layered split; there is no current `aura-executor` crate. Execution is now spread across the exec-layer crates (`aura-exec-*`, `aura-tools`) and the agent-layer kernel traits (`aura-agent-kernel`, re-exported by `aura-kernel`).

- [x] `Executor` trait definition
- [x] `ExecuteContext` struct
- [x] `ExecuteLimits` struct
- [x] `ExecutorRouter` implementation
- [x] Action dispatch by kind
- [x] `NoOpExecutor` stub
- [x] Unit tests

---

### Phase 4: Tools (`aura-tools`)
**Status:** 🟢 Complete (code written, requires LLVM to build)

Filesystem and command tools with sandbox.

- [x] `ToolCall` struct (in aura-core)
- [x] `ToolResult` struct (in aura-core)
- [x] `ToolExecutor` implementation
- [x] `fs.ls` - directory listing
- [x] `fs.read` - file read with limits
- [x] `fs.stat` - file metadata
- [x] Sandbox path validation
- [x] Path canonicalization + prefix check
- [x] `cmd.run` - command execution (disabled by default)
- [x] Timeout enforcement structure
- [x] Output size limits
- [x] Unit tests for path traversal prevention

---

### Phase 5: Reasoner Client (`aura-reasoner`)
**Status:** 🟢 Complete

HTTP client abstraction. The early TypeScript sidecar target was removed; production model calls now use the Rust `reqwest` provider through the JWT-authenticated `aura-router` proxy.

- [x] `Reasoner` trait definition
- [x] `ProposeRequest` struct
- [x] `RecordSummary` struct
- [x] `ReasonerConfig` struct
- [x] HTTP client implementation (reqwest)
- [x] Timeout + retry logic
- [x] Error handling
- [x] `MockReasoner` for testing
- [x] Unit tests

---

### Phase 6: Kernel (`aura-kernel`)
**Status:** 🟢 Complete (code written, requires LLVM to build)

Deterministic kernel with context building and policy.

- [x] `Kernel` struct
- [x] `KernelConfig` struct
- [x] Context builder (record window)
- [x] `context_hash` computation
- [x] Policy engine (`Policy` struct)
- [x] Action kind allowlist
- [x] Tool allowlist
- [x] Proposal → Action conversion
- [x] Execution orchestration
- [x] `RecordEntry` construction
- [x] Replay mode (skip Reasoner/Tools)
- [x] Unit tests for determinism
- [x] Unit tests for policy enforcement

---

### Phase 7: Runtime Gateway + Engine Split (`aura-runtime`, `aura-engine`)
**Status:** 🟢 Complete (code written, requires LLVM to build)

HTTP/WS gateway, scheduler, and worker management. The original scheduler/worker implementation later moved out of `aura-runtime` into the surface-layer `aura-engine` orchestration crate; `aura-runtime` is now the gateway and `aura-node` composition root.

- [x] Axum HTTP router setup
- [x] `POST /tx` endpoint
- [x] `POST /v1/run` endpoint returning `RuntimeRunResponse`
- [x] `WS /stream/:run_id` per-run event stream
- [x] `GET /agents/{id}/head` endpoint
- [x] `GET /agents/{id}/record` endpoint
- [x] `GET /health` endpoint
- [x] Per-agent processing claim in `aura-engine::Scheduler`
- [x] Worker loop implementation in `aura-engine::process_agent`
- [x] HTTP `DomainApi` implementation in `aura-domain-http`
- [x] Concrete `FleetSubagentDispatcher` in `aura-fleet-subagent`
- [x] `NodeConfig` struct

---

### Phase 8: TypeScript Gateway (`aura-gateway-ts`) — REMOVED

The TypeScript sidecar that originally wrapped the Claude Code SDK has been removed. `aura-reasoner` now calls Anthropic-shaped LLMs in Rust (`reqwest`) through `aura-router` (the AURA proxy) using a per-request JWT. The legacy direct-Anthropic path was deleted in the proxy-only collapse — prompt caching, tool schemas, and the propose-only contract are all implemented in-tree. See Phase 16 for the deprecation/removal record.

---

### Phase 9: Integration & Testing
**Status:** 🟢 Complete / ongoing CI gate

End-to-end tests and verification. The workspace now has CI for formatting, clippy, tests, MSRV, dependency policy, advisory scans, unused dependency checks, and invariant band checks.

- [x] Full pipeline test coverage (tx/run → record/events)
- [x] Determinism test coverage (replay)
- [x] Atomicity test coverage
- [x] Concurrency test coverage
- [x] Tool sandbox test coverage (path traversal)
- [ ] Performance benchmarks (optional)

---

## Spec-02: Interactive Coding Runtime (Rust-only)

### Phase 10: Provider Abstraction (`aura-reasoner` refactor)
**Status:** 🟢 Complete

Provider-agnostic model interface.

- [x] Define normalized `Message`, `ContentBlock` types
- [x] Define `ToolDefinition` struct (JSON Schema)
- [x] Define `ModelRequest` / `ModelResponse` structs
- [x] Define `ModelProvider` trait
- [x] Update mock provider to implement `ModelProvider`
- [x] Collapse production provider selection to the JWT-authenticated `aura-router` path

---

### Phase 11: Router-Backed Anthropic-Compatible Provider
**Status:** 🟢 Complete

Anthropic-compatible streaming over the Aura router proxy. The historical direct-Anthropic provider plan was superseded by the proxy-only collapse; production traffic no longer calls Anthropic directly from this workspace.

- [x] Implement proxy-backed `AnthropicProvider` over `reqwest`
- [x] AURA → Anthropic-shaped request conversion
- [x] Anthropic-shaped SSE → AURA stream conversion
- [x] Tool schema conversion
- [x] Unit tests with mock responses

---

### Phase 12: Tool Catalog
**Status:** 🟢 Complete

Centralized tool definitions with JSON Schema.

- [x] Define `ToolCatalog`
- [x] Use catalog metadata as the tool source of truth
- [ ] JSON schemas for: fs.ls, fs.read, fs.stat, fs.write, fs.edit
- [ ] JSON schema for: search.code (ripgrep)
- [ ] JSON schema for: cmd.run (gated)
- [x] Tri-state tool state mapping

---

### Phase 13: AgentLoop Orchestration (was Turn Processor)
**Status:** 🟢 Complete

Multi-step agentic conversation loop (sole orchestrator).

- [x] `AgentLoop` struct with `AgentLoopConfig` (replaced original `TurnProcessor` design)
- [x] Conversation loop (model → tool_use → tool_result → repeat) with streaming
- [x] Tool execution via `KernelToolExecutor` (parallel mode, per-tool timeouts, policy deny)
- [x] Tool result caching, blocking detection, stall detection
- [x] Budget enforcement (max iterations, credit budget, exploration allowance)
- [x] Timeout handling, cancellation support
- [x] Context compaction and thinking taper
- [x] `TurnEvent` unified streaming events (including `StreamReset` for fallback determinism)

---

### Phase 14: Permission System
**Status:** 🔴 Not Started

Approval flow for sensitive operations.

- [x] Replace `PermissionLevel` with tri-state `ToolState`
- [ ] Default permission mapping per tool
- [ ] Approval request generation
- [ ] Approval response handling
- [x] Session-scoped live ask decisions

---

### Phase 15: CLI (root `aura` binary + `aura-surface-cli`)
**Status:** ⛔ Superseded (2026, Wave 4 refactor)

Interactive command-line interface. **The separate `aura-cli` crate
was never created.** Its intended surface is now delivered by the
root `aura` binary (`src/`) — interactive TUI, login / logout /
whoami, and the embedded HTTP server for file / record access. The
headless server half lives in `aura-runtime`. See
[`README.md`](../README.md) under "Binaries" for the canonical entry
point.

- [x] ~~Create `aura-cli` crate~~ — dropped; root `aura` binary
  is a thin shim into `aura_surface_cli::run`.
- [x] REPL loop with prompt — delivered by the ratatui TUI in
  `src/event_loop/` and `aura-terminal`.
- [x] Transaction submission — delivered by `aura run` / the TUI's
  session bootstrap.
- [x] Record streaming / tailing — delivered by the `/stream`
  WebSocket in `aura-runtime`.
- [x] Slash commands (/status, /history, /approve, /deny) — TUI
  command palette / event loop.
- [x] Approval prompts inline — TUI approval modal.

---

### Phase 16: Gateway Deprecation
**Status:** 🟢 Complete

TypeScript gateway dependency removed.

- [x] Provider selection config — collapsed in 2026: only the `aura-router` proxy path remains; LLM auth is JWT-only.
- [x] Rust provider tested end-to-end (`AnthropicProvider` + mock)
- [x] Rust provider is the only path (Node sidecar deleted)
- [x] `aura-gateway-ts` directory removed from the workspace
- [x] Documentation updated (README, PROGRESS, v0.1.0/v0.1.1 specs)

---

## Legend

- 🔴 Not Started
- 🟡 In Progress
- 🟢 Complete
- ⏸️ Blocked

---

## Crate Structure

```
aura-harness/
├── Cargo.toml           # Workspace manifest + root `aura` binary
├── rust-toolchain.toml  # Stable local toolchain; MSRV lives in Cargo.toml
├── src/                 # Root `aura` binary shim into aura-surface-cli
├── crates/              # 58 workspace crates in the 10-layer architecture
│   ├── aura-core-*      # Core IDs, modes, permissions, auth, protocol primitives
│   ├── aura-store-*     # RocksDB store, record log, snapshots
│   ├── aura-config      # Env/TOML configuration source of truth
│   ├── aura-model-*     # Router-backed LLM provider abstraction
│   ├── aura-context-*   # Prompts, memory, compaction, skills
│   ├── aura-plugin-*    # Plugin API, manifests, hooks, MCP, connectors
│   ├── aura-exec-*      # Tool execution, sandbox, policy, isolation, conflicts
│   ├── aura-agent-*     # Deterministic kernel, AgentLoop, steering, subagents
│   ├── aura-fleet-*     # Registry, quota, spawn, dispatch, mailbox, daemon, subagent dispatcher
│   └── aura-surface-*   # CLI, SDK, terminal, automaton, auth surface shells
├── crates/aura-runtime/ # Sole external Cargo surface; HTTP/WS gateway + aura-node root
├── crates/aura-engine/  # Orchestration engine: scheduler, workers, automatons, child runner
├── crates/aura-domain-http/
│                         # HTTP DomainApi implementation + JWT wrapper
├── tests/               # Workspace integration, boundary, e2e, proptest tests
├── docs/                # Architecture, invariants, progress, refactoring notes
├── scripts/             # Invariant checks and helpers
└── .github/             # CI, invariants, security workflows
```

> **Historical note (2026):** this tree previously listed
> `aura-executor/` and `aura-cli/`. `aura-executor` was dissolved into
> the current exec / agent-layer crates. `aura-cli` was never created —
> its surface is the root `aura` binary (`src/`) plus `aura-surface-cli`.
> The `aura-node` crate was renamed to `aura-runtime`; the binary name
> remains `aura-node`.

---

## Notes

### 2026-04-24: System-Audit Refactor (Phases 0-6)

Second pass over the codebase, narrower than the original
`aura-executor` dissolution. Driven by the plan in
`C:\Users\n3o\.cursor\plans\system-audit-refactor_c3234749.plan.md`.
The full close-out checklist is in
[`docs/refactoring/phase-checklist.md`](refactoring/phase-checklist.md) §5.
One-paragraph summary per phase:

- **Phase 0 — Invariant gating + crate rename.** Routed the HTTP
  `tool_permissions` PUT handler under the per-agent scheduler lock so
  its `append_entry_direct` call is correctly serialized with the
  kernel's own writes. Renamed the `aura-node` crate to `aura-runtime`
  to match the layered-architecture vocabulary while keeping the
  binary name (`aura-node.exe`) stable for operators. Wired
  `scripts/check_invariants.sh` into CI via
  `.github/workflows/invariants.yml` so future drift fails review.
- **Phase 1 — Sole external gateway hardening.** Introduced
  `KernelDomainGateway` (in `aura-agent`) so every automaton/agent
  domain mutation routes through `Kernel::process_direct` and produces
  a `System/DomainMutation` `RecordEntry`. Added the `await` on
  `scheduler.schedule_agent` inside `AutomatonBridge::record_lifecycle_event`
  so lifecycle entries reliably commit instead of sitting in the
  inbox. Closed the §3 gap on sync + handshake reasoning failures —
  both now record a `Reasoning` `RecordEntry`.
- **Phase 2a — God-module splits in `aura-core` / `aura-kernel`.**
  `types/tool.rs` → `types/tool/` (proposal, execution, installed,
  runtime_capability, call, result). `policy/check.rs` →
  `policy/check/` (delegate_gate, agent_permissions, integration_gate,
  scope, verdict, tests). `kernel/tools.rs` → `kernel/tools/`
  (single, batch, shared). `context.rs` → `context/` to lift the
  ~400 lines of `#[cfg(test)]` out of the production module.
- **Phase 2b — God-module splits in `aura-tools` / `aura-reasoner`.**
  `resolver/trusted.rs` → `resolver/trusted/` with
  `integrations/{github,linear,slack,resend,brave}.rs`.
  `git_tool/mod.rs` → per-subcommand modules (`executor`, `sandbox`,
  `commit`, `push`, `commit_push`, `redact`, `tests`).
  `anthropic/sse.rs` → `anthropic/sse/{parse,event,state,tests}.rs`.
- **Phase 2c — God-module splits in `aura-runtime` / `aura-agent`.**
  `automaton_bridge.rs` → `automaton_bridge/` (`mod`, `build`,
  `event_channel`, `dispatch`, `tests`). `kernel_domain_gateway.rs`
  → `kernel_domain_gateway/` (`specs`, `project`, `storage`, `orbit`,
  `network`, `tasks`, `tests`).
- **Phase 3 — Shared embedder bootstrap + event mapping.** Pulled
  the duplicated TUI / node startup glue into
  `aura_agent::session_bootstrap`; `src/session_helpers.rs` is now a
  thin re-export. Introduced `aura_agent::events::TurnEventSink` plus
  `map_agent_loop_event` so the TUI `UiCommandSink` and the node
  `OutboundMessageSink` share one mapping. Pulled the workspace
  walker / capped reader into `aura_runtime::files_api` so both the
  node `/api/files` handlers and the TUI-embedded `src/api_server.rs`
  go through the same code.
- **Phase 4 — Type-state seal + mid-loop refactor.** Introduced the
  sealed `aura_agent::RecordingModelProvider` marker so automatons
  take `P: RecordingModelProvider` rather than
  `Arc<dyn ModelProvider>`; this locks Invariant §1 ("Sole External
  Gateway") into the type system. Renamed the kernel-internal
  `ToolDecision` to `ToolGateVerdict` to disambiguate it from the
  `aura_core` audit-log enum. Split `agent_loop/iteration.rs` into
  the `iteration/` directory and introduced `IterCounters` /
  `ThinkingBudget`. Renamed `tool_processing` → `tool_pipeline` and
  extracted `ToolResultCache`. Split `aura_agent::events` and
  `aura_runtime::router::memory` along the `types`/`wire`/`handlers`/
  `tests` axis. Split `dev_loop` into `aggregate.rs`,
  `forward_event.rs`, and `validation.rs`.
- **Phase 5 — Test-only reachability cleanup.** Gated
  test-only constructors and helpers behind `#[cfg(test)]`
  consistently and reduced unused-import / dead-code warnings to
  zero under `--all-features --all-targets`.
- **Phase 6 — Finish & document (this checkpoint).** Wired the
  policy-derived `thinking_budget` through
  `AgentLoopConfig::thinking_budget` into `LoopState::thinking.budget`
  (capped at `max_tokens`). Tightened `aura_kernel::router::ExecutorRouter::execute`:
  multiple matching executors now `error!`-log, panic under
  `debug_assert!` in debug/test builds, and return
  `Effect::Failed("ambiguous executor routing")` in release. Refreshed
  `scripts/check_invariants.sh` §2 + §10 allowlists for the Phase 2c
  module layout (directory-prefix forms for `automaton_bridge/` and
  `kernel_domain_gateway/`, `router/state.rs` for the `Arc<dyn Store>`
  RouterState field, and `tool_permissions.rs` as the sanctioned
  HTTP-driven append site). Updated `docs/architecture.md`,
  `docs/invariants.md`, `docs/refactoring/phase-checklist.md`,
  `README.md`, and this file accordingly.

### 2026-01-08: Initial Implementation

- Created full workspace structure with 7 Rust crates
- Implemented all core types with serialization
- Implemented RocksDB store with atomic WriteBatch commits
- Implemented executor framework with tool executor
- Implemented sandboxed filesystem tools (ls, read, stat)
- Implemented reasoner client with mock for testing
- Implemented deterministic kernel with policy engine
- Implemented swarm runtime with HTTP API and scheduler
- Build verified for the then-current non-native crates. Historical references to
  `aura-executor` now map to the exec-layer crates plus `aura-agent-kernel`.
- RocksDB crates require LLVM/Clang installation on Windows

### Key Design Decisions

1. **Atomic Commits**: All state changes use RocksDB WriteBatch for atomicity
2. **Per-Agent Locking**: DashMap with Mutex ensures single-writer per agent
3. **Replay Mode**: Kernel can skip reasoner/tools for deterministic replay
4. **Sandbox**: All tool paths are canonicalized and validated against workspace root
5. **Policy Engine**: Allowlists for action kinds and tools, applied deterministically
