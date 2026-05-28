# Changelog

All notable changes to the Aura agent-first architecture refactor
are tracked here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Architecture refactor — Phases 1 through 9

The architecture refactor lands the 10-layer crate stack (`core` →
`store` → `config` → `model` → `context` → `plugin` → `exec` →
`agent` → `fleet` → `surface`) with strict downward-only
dependencies enforced by `tests/layer_boundary.rs`. Every phase
left the workspace `cargo fmt` / `cargo clippy -D warnings` /
`cargo test --workspace` green.

- **Phase 1** — `refactor(arch-phase-1)` (`1851ad6`): split
  `aura-core` into `aura-core-types`, `aura-core-protocol`,
  `aura-core-permissions`, `aura-core-modes`, and `aura-core-auth`;
  introduce the closed `AgentMode` / `KernelMode` / `SpawnMode` /
  `JoinPolicy` / `ReplayMode` / `SandboxMode` enums; add
  advisory `tests/layer_boundary.rs`.
- **Phase 2** — `refactor(arch-phase-2)` (`7307b01`): split the
  store layer into `aura-store-db`, `aura-store-record`,
  `aura-store-snapshot` (the snapshot crate ships as a V1 no-op
  stub); preserve `aura-store` as a compatibility shell.
- **Phase 3** — `refactor(arch-phase-3)` (`6d2f69d`): rename the
  model + context crates into the layered `aura-model-reasoner`,
  `aura-context-prompts`, `aura-context-memory`,
  `aura-context-compaction`, `aura-context-skills`; add the
  `aura-agent-steering` placeholder.
- **Phase 4a** — `refactor(arch-phase-4a)` (`e15ff72`): extend
  `aura-config` with the fleet / subagent / conflict / plugins
  TOML tables, the `AURA_HOME` resolution helper, and the
  `aura migrate` stub.
- **Phase 4b** — `refactor(arch-phase-4b)` (`e8108ce`): plugin
  schema + install + cache + marketplace + `aura plugins` CLI;
  introduces `aura-plugin-api` (in-process contributor traits)
  and `aura-plugin-core` (declarative manifest pipeline).
- **Phase 4c** — `refactor(arch-phase-4c)` (`89201f6`): plugin
  runtime surfaces — `aura-plugin-hooks` (hook engine + 10
  Codex/Claude lifecycle events), `aura-plugin-mcp` (stdio
  JSON-RPC client), `aura-plugin-connectors`, plus the trust
  prompt flow.
- **Phase 5** — `refactor(arch-phase-5)` (`70f18ee`): exec layer
  split — `aura-exec-conflict`, `aura-exec-isolation`,
  `aura-exec-policy`, `aura-exec-sandbox`, `aura-exec-tools`,
  `aura-exec-runner`; introduce `ConflictRegistry` and
  `WorktreeIsolation`.
- **Phase 6a** — `refactor(arch-phase-6a)` (`a316784`) +
  follow-up `refactor(arch-phase-6a-audit)` (`5a220dc`): agent
  layer split — `aura-agent-kernel`, `aura-agent-loop`,
  `aura-agent-subagent`, `aura-agent-steering`; introduces the
  `KernelMode::{Audited, AuditedLite}` tiering.
- **Phase 6b** — `refactor(arch-phase-6b)` (`48356ba`): replay
  wiring — `ReplayConsumer`, `ReplayError`, `RecordLog::scan`,
  `KernelConfig::replay_from`.
- **Phase 6c** — `refactor(arch-phase-6c)` (`8aa9441`):
  context-memory inversion — `TurnSummary` + `ModelProvider`
  injection breaks the four remaining `aura-context-memory ->
  aura-agent` upward edges; `MemoryTurnObserver` relocated to
  `aura-runtime`; first warn-only edge promoted to fail-on-detect.
- **Phase 7a** — `refactor(arch-phase-7a)` (`682e888`): fleet
  daemon composition root — `aura-fleet-registry`,
  `aura-fleet-spawn`, `aura-fleet-dispatch`, `aura-fleet-quota`,
  `aura-fleet-daemon`; per-parent audit-append lease replaces
  the old single `spawn_lock`; task tool routed through
  `aura-agent-subagent::derive_subagent ->
  aura-fleet-spawn::spawn`.
- **Phase 7b** — `refactor(arch-phase-7b)` (`5f90526`): full
  `SpawnMode::{Detached, Batch}` + `aura-fleet-mailbox` +
  `JoinPolicy` enforcement + quota enforcement + orphan handoff
  on parent death + the complete `SubagentOverrides` /
  `OverrideManifest` validation surface.
- **Phase 8** — `refactor(arch-phase-8)` (`dcfd0c9`): plugin
  runtime integration end-to-end — skills wired into
  `aura-context-skills`, MCP merge into `aura-plugin-mcp`, hooks
  into `aura-plugin-hooks`, connectors into
  `aura-plugin-connectors`; hook events fired at every documented
  agent-loop / fleet-spawn lifecycle point; hook sandbox env
  scrubbing; backward compat verified (empty `~/.aura/plugins/`
  yields zero behavioural diff).
- **Phase 9** — `refactor(arch-phase-9)`: surface layer + SDK +
  strict layer boundary enforcement. See the per-section list
  below.

### Added — Phase 9

- New `aura-surface-cli` crate at the `surface` layer hosting the
  CLI composition-root types (`ModeFlag`, `parse_mode_str`,
  `CliModeInputs`, `version_banner`).
- New `aura-surface-sdk` crate exposing the external
  `AuraClient` / `AuraSession` shape over `aura-core-protocol`,
  with the documented `SessionConfig::mode: Option<AgentMode>`
  field that feeds the AgentMode resolution priority chain.
- New `aura-surface-terminal` relocation shell over the legacy
  `aura-terminal` crate. Adds the typed `SlashModeCommand` for
  parsing `/mode <agent|plan|ask|debug>` from the TUI input
  layer.
- New `aura-surface-automaton` relocation shell over the legacy
  `aura-automaton` crate.
- New `aura-surface-auth` relocation shell over the legacy
  `aura-auth` crate (`ZosClient`, `CredentialStore`). The token
  primitive types (`AccessToken`, `RefreshToken`, `Token`,
  `StoredSession`, `AuthError`) stay at the `core` layer in
  `aura-core-auth` and are re-exported through the surface
  shell.
- `FleetConfig::default_mode: AgentMode` field — the daemon
  default rung of the documented `AgentMode` resolution priority
  chain. Overridable via the new `AURA_FLEET_DEFAULT_MODE`
  environment variable.
- `aura_fleet_daemon::resolve_session_mode` helper +
  `AgentModeInputs` struct implementing the documented priority:
  CLI flag > TUI slash > SDK field > daemon default >
  `AgentMode::Agent` fallback. Children inherit the resolved
  mode through the existing `derive_subagent` chain (narrowing
  only).
- Top-level `aura --mode <agent|plan|ask|debug>` global CLI flag
  (clap-derived `ModeFlag` from `aura-surface-cli`).
- `tests/layer_boundary.rs` strict-mode promotion. The
  `WARN_ONLY_UPWARD_EDGES` allowlist now carries a single
  explicitly-documented Phase 10 follow-up entry
  (`aura-tools -> aura-kernel`); every other upward edge fails
  CI. Phase 9 also introduces the
  `every_crate_carries_a_matching_layer_doc_tag` test that
  parses each `crates/<name>/src/lib.rs` for the
  `//! Layer: <layer>` doc-comment and asserts consistency with
  the `KNOWN_CRATES` table.
- `crates/aura-fleet-daemon/tests/mode_resolution_priority.rs`
  with an `insta` snapshot pinning the priority order across
  ten input combinations.
- New `machete` job in `.github/workflows/ci.yml` running
  `cargo machete` against the workspace.

### Changed — Phase 9

- `aura-agent` no longer depends on `aura-auth`.
  `default_agent_config(model)` now delegates to the new
  `default_agent_config_with_auth(model, auth_token)` helper;
  surface-layer callers (`src/session_helpers.rs`) compose the
  env-var lookup (`load_auth_token`) with the credential-store
  lookup (`aura_auth::CredentialStore::load_token`) before
  invoking it.
- `aura-auth` reclassified from `core` to `surface` in
  `KNOWN_CRATES` since `ZosClient` (HTTP) and `CredentialStore`
  (OS keyring) are surface-layer concerns per the plan's
  cross-cutting "Secrets" ownership bullet.
- `deny.toml` finalised: explicit MIT / Apache-2.0 / BSD / ISC
  / MPL / Unicode allow-list; 30-day advisory window enforced
  via the `[advisories]` block (paired with the documented
  `audit` job soft-fail in `.github/workflows/ci.yml`);
  crates.io-only sources. The `openssl-sys` ban is documented
  as a TODO (the workspace's `reqwest 0.11` still pulls in
  native-tls today; flipping to `rustls-tls` is tracked as a
  Phase 10 follow-up). Duplicate-major detection for `tokio` /
  `serde` runs via the existing `multiple-versions = "warn"` +
  `highlight = "all"` toggle.
- `aura --help` now lists the top-level `--mode` global flag.
- The `audit` CI job retains `continue-on-error: true` (the
  hard-fail gate is `cargo deny check`'s 30-day advisory window
  in `deny.toml`); the YAML now carries an explanatory comment.
- `docs/architecture.md` carries a Phase 9 layer overview +
  refreshed per-layer crate inventory.

### Deprecated — Phase 9

- `aura_agent::session_bootstrap::load_auth_token` no longer
  reads the OS keyring; surface-layer callers chain
  `aura_auth::CredentialStore::load_token` explicitly. The
  agent-layer function remains for the env-var path.

### Notes — Phase 9 follow-ups

- `WARN_ONLY_UPWARD_EDGES` retains a single
  `aura-tools -> aura-kernel` entry. The clean fix is to
  relocate `ExecuteContext` / `Executor` / `ExecutorError` /
  `SpawnHook` out of `aura-agent-kernel` to the exec layer.
  Tracked as a Phase 10 follow-up; documented in the test
  source.
- The full body migration of `src/main.rs` (the `aura` binary)
  and `crates/aura-runtime/src/main.rs` (the `aura-node`
  binary) into `aura-surface-cli::run` is incremental. Phase 9
  ships the surface-layer type and dependency topology;
  Phase 10 lifts the binary bodies.
- The `openssl-sys` ban in `deny.toml` is documented as a TODO
  pending the workspace migration to `reqwest`'s `rustls-tls`
  feature.
