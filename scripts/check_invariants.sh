#!/usr/bin/env bash
# check_invariants.sh — rg-band enforcement for architectural invariants
# §1, §2, §3, §9, §10 (see `docs/invariants.md` Part A / Part C).
#
# This script is executed from CI (`.github/workflows/invariants.yml`) and
# can be run locally: `bash scripts/check_invariants.sh`. It uses ripgrep
# to detect forbidden code patterns outside their allowed modules and
# fails with exit code 1 on the first violation.
#
# Active crate layout (post-Phase-10):
#   - kernel: `aura-agent-kernel/` (active) + `aura-kernel/` (Phase 6a shell)
#   - store:  `aura-store-db/` (active) + `aura-store/` (Phase 2 shell) +
#             `aura-store-record/` (per-agent RecordLog trait) +
#             `aura-store-snapshot/` (replay snapshot trait)
#   - reasoner: `aura-model-reasoner/` (active) + `aura-reasoner/` (shell)
#   - memory:   `aura-context-memory/` (active) + `aura-memory/` (shell)
#
# When a legitimate new call-site needs to land (e.g. a newly-approved
# gateway that routes `.complete(` through the kernel, or a fleet-layer
# crate that holds a store handle for a kernel-mediated audit write), add
# its path to the corresponding allowlist regex below AND update the
# matching section of `docs/invariants.md` in the same PR.

set -euo pipefail

if ! command -v rg >/dev/null 2>&1; then
    echo "error: ripgrep (rg) is required to run the invariant check." >&2
    exit 2
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

violations=0

# Emit a violation message and mark the run as failed without aborting
# the rest of the checks — we surface every band in one run.
report() {
    local invariant="$1"; shift
    local description="$1"; shift
    local match_file="$1"; shift
    echo "::error file=${match_file}::Invariant ${invariant} violation (${description}): ${match_file}"
    violations=$((violations + 1))
}

# Run `rg` against the repo, stream each match through an allowlist
# regex, and report anything left as a violation. Using a PCRE-ish
# allowlist keeps the matrix readable and avoids --glob explosions.
run_band() {
    local invariant="$1"; shift
    local description="$1"; shift
    local pattern="$1"; shift
    local allow_regex="$1"; shift

    # `|| true` so rg's own exit code (1 on zero matches) doesn't trip `set -e`.
    local raw
    raw=$(rg -n --hidden --glob '!target/**' --glob '!.git/**' --type rust "$pattern" . || true)
    if [[ -z "$raw" ]]; then
        return 0
    fi

    while IFS= read -r line; do
        # Strip line/column prefix to get the path.
        local path="${line%%:*}"
        # Normalize Windows `rg` output (`.\crates\...`) so allowlists stay
        # POSIX-shaped and match the paths emitted in CI.
        path="${path//\\//}"
        path="${path#./}"
        if [[ "$path" =~ $allow_regex ]]; then
            continue
        fi
        report "$invariant" "$description" "$line"
    done <<<"$raw"
}

# §1/§3 — `.complete(` must only appear inside:
#   - the kernel itself (`aura-agent-kernel/`; `aura-kernel/` is a Phase 6a
#     re-export shell)
#   - the agent-side recording seams (kernel_gateway.rs, recording_stream.rs)
#   - the reasoner provider internals and their mocks (`aura-model-reasoner/`;
#     `aura-reasoner/` is the Phase 3 re-export shell)
#   - the automaton runtime (wraps its provider with KernelModelGateway before
#     handing it over)
#   - the memory subsystem, which only ever holds an Arc<KernelModelGateway>
#     (`aura-context-memory/`; `aura-memory/` is the Phase 3 shell)
#   - any *test* file (unit, integration, harness shims)
run_band "§1/§3" "direct ModelProvider::complete call outside the recording seam" \
    '\.complete\(' \
    '^(crates/aura-agent-kernel/|crates/aura-kernel/|crates/aura-agent/src/kernel_gateway\.rs|crates/aura-agent/src/recording_stream\.rs|crates/aura-agent/src/agent_loop/|crates/aura-agent/src/event_sequence_tests\.rs|crates/aura-model-reasoner/|crates/aura-reasoner/|crates/aura-automaton/|crates/aura-context-memory/|crates/aura-memory/|.*/tests/|.*test.*\.rs|.*tests.*\.rs)'

# §2 — direct store append functions bypass the per-agent kernel's
# seq/context-hash pipeline unless they are inside:
#   - the active kernel impl (`aura-agent-kernel/`; `aura-kernel/` is the
#     Phase 6a re-export shell)
#   - the active store impl (`aura-store-db/`; `aura-store/` is the Phase 2
#     re-export shell)
#   - the abstract per-agent `RecordLog` trait (`aura-store-record/`)
#   - tests
#
# Sanctioned non-kernel/store call-sites (documented as exceptions in
# docs/invariants.md):
#   * `crates/aura-runtime/src/tool_permissions.rs` — HTTP-driven tool-
#     permissions append guarded by the per-agent scheduler claim (see
#     §12.a). Routed via `aura_agent_kernel::write_system_record`.
# Fleet-spawn `SubagentSpawn` audit rows and `promote_to_orphan` writes
# also go through `aura_agent_kernel::write_system_record`, which calls
# `WriteStore::append_entry_direct` internally — `aura-fleet-spawn/` itself
# never names an `append_entry_*` symbol, so no allowlist entry is needed.
run_band "§2" "append_entry_* used outside the kernel / store crates / tests" \
    'append_entry_(atomic|dequeued|direct|entries_batch)' \
    '^(crates/aura-agent-kernel/|crates/aura-kernel/|crates/aura-store-db/|crates/aura-store/|crates/aura-store-record/|crates/aura-runtime/src/tool_permissions\.rs|.*/tests/|.*test.*\.rs|.*tests.*\.rs)'

# §1 — raw `git` processes must live in a kernel-mediated executor or in a
# declared infrastructure exception.
#
# Permitted locations for `Command::new("git")`:
#
#   * `crates/aura-tools/src/git_tool/` — the `GitExecutor` and its shared
#     helpers (`git_commit_impl`, `git_push_impl`, `git_commit_push_impl`).
#     Every mutating `git` subprocess in the tree must funnel through here.
#   * `crates/aura-agent/src/git.rs` — read-only helpers (`git log` for
#     unpushed-commit telemetry). Declared exception in docs/invariants.md.
#   * `crates/aura-exec-isolation/src/lib.rs` — `git worktree add` /
#     `worktree remove` for parallel-safe subagent workspaces. Sandbox /
#     isolation infrastructure, not a mutating commit/push. Declared
#     exception in docs/invariants.md.
#   * Test files.
run_band "§1" "Command::new(\"git\") outside the GitExecutor / declared exceptions" \
    'Command::new\("git"\)' \
    '^(crates/aura-tools/src/git_tool/|crates/aura-agent/src/git\.rs|crates/aura-exec-isolation/src/lib\.rs|.*/tests/|.*test.*\.rs|.*tests.*\.rs)'

# §10 — non-kernel, non-store crates must bind to `Arc<dyn ReadStore>`.
#
# `Arc<dyn Store>` exposes the sealed `WriteStore` surface. It is only
# legitimate in:
#
#   * The active kernel + store impl crates (`aura-agent-kernel`,
#     `aura-store-db`, `aura-store-record`) and their Phase-2/6 re-export
#     shells (`aura-kernel`, `aura-store`).
#   * Test scaffolding (`mod tests`, `tests/`, `*_tests.rs`).
#   * A bounded set of production sites that must hand a store handle to a
#     per-agent `Kernel::new` (which still takes `Arc<dyn Store>`). Each is
#     listed explicitly below. Once the kernel accepts a
#     `(ReadStore, WriteHook)` pair, this allowlist collapses to just the
#     kernel/store crates.
#
# Pattern also accepts `aura_store_db::Store` (the active impl crate's
# explicit path) so post-Phase-2 callsites stay covered.
#
# Production holders:
#   - crates/aura-runtime/src/gateway/state.rs   — RouterState field piped into WsContext
#   - crates/aura-runtime/src/gateway/session/mod.rs — WsContext handed to Kernel::new
#   - crates/aura-runtime/src/gateway/session/cross_agent_hook.rs — cross-agent chat/spawn hook
#   - crates/aura-runtime/src/gateway/session/child_kernel.rs — session-scoped child-kernel
#                                                  factory captures the same store handle as
#                                                  WsContext so nested child runs route through
#                                                  RuntimeChildRunner / Scheduler.
#   - crates/aura-engine/src/scheduler.rs        — Scheduler builds per-agent kernels (§12.a claim seam)
#   - crates/aura-engine/src/automaton/          — AutomatonBridge builds per-agent automaton kernels
#                                                  (mod.rs + build.rs + dispatch.rs + event_channel.rs)
#   - crates/aura-engine/src/child_runner.rs     — RuntimeChildRunner creates child kernels
#                                                  through the scheduler
#   - crates/aura-runtime/src/node.rs            — boots the process-wide store and wires runtime surfaces
#   - crates/aura-runtime/src/tool_permissions.rs — HTTP-driven tool-permissions
#                                                  append serialized via the §12.a claim
#   - crates/aura-fleet-daemon/src/lib.rs        — FleetDaemon::new holds the store handle
#                                                  passed to FleetSpawner
#   - crates/aura-fleet-spawn/src/spawner.rs     — FleetSpawner holds the store for
#                                                  write_system_record SubagentSpawn audit rows
#                                                  (§12.b ParentLeaseRegistry serialises these)
#   - crates/aura-fleet-subagent/src/dispatch.rs — FleetSubagentDispatcher hands the store to FleetSpawner
#   - crates/aura-surface-cli/src/runner.rs      — interactive TUI composition root constructs a Kernel
#
# Test-only holders (filenames that don't match `*test*.rs` but whose hits
# are inside `#[cfg(test)] mod tests`):
#   - crates/aura-agent/src/kernel_gateway.rs
#   - crates/aura-agent/src/kernel_domain_gateway/
#   - crates/aura-agent/src/recording_stream.rs
#   - crates/aura-engine/src/worker.rs
run_band "§10" "Arc<dyn Store> outside the kernel / store crates" \
    'Arc<dyn (aura_store(_db)?::)?Store>' \
    '^(crates/aura-agent-kernel/|crates/aura-kernel/|crates/aura-store-db/|crates/aura-store/|crates/aura-store-record/|crates/aura-runtime/src/gateway/state\.rs|crates/aura-runtime/src/gateway/session/mod\.rs|crates/aura-runtime/src/gateway/session/cross_agent_hook\.rs|crates/aura-runtime/src/gateway/session/child_kernel\.rs|crates/aura-engine/src/scheduler\.rs|crates/aura-engine/src/automaton/|crates/aura-engine/src/child_runner\.rs|crates/aura-engine/src/worker\.rs|crates/aura-runtime/src/node\.rs|crates/aura-runtime/src/tool_permissions\.rs|crates/aura-fleet-daemon/|crates/aura-fleet-spawn/|crates/aura-fleet-subagent/src/dispatch\.rs|crates/aura-surface-cli/src/runner\.rs|crates/aura-agent/src/kernel_gateway\.rs|crates/aura-agent/src/kernel_domain_gateway/|crates/aura-agent/src/recording_stream\.rs|crates/aura-agent/src/agent_loop/|crates/aura-memory/src/test_kernel\.rs|.*/tests/|.*test.*\.rs|.*tests.*\.rs)'

# §9 — the agent loop must not reach into the store crates directly. Any
# code that needs persistence goes through the per-agent kernel. Test files
# in the same tree are exempt since they assemble scaffolding.
#
# The pattern catches both `use aura_store_db::` (the Phase 2 re-export shell)
# and `use aura_store_db::` (the active impl crate).
store_hits=$(rg -n --hidden --glob '!target/**' --glob '!.git/**' --type rust \
    --glob 'crates/aura-agent/src/agent_loop/**' \
    --glob '!**/*test*.rs' --glob '!**/*tests*.rs' \
    'use aura_store(_db)?::' . || true)
if [[ -n "$store_hits" ]]; then
    while IFS= read -r line; do
        report "§9" "aura-agent/agent_loop must not depend on the store crates" "$line"
    done <<<"$store_hits"
fi

if (( violations > 0 )); then
    echo ""
    echo "Invariant band check failed with ${violations} violation(s)." >&2
    echo "See docs/invariants.md and scripts/check_invariants.sh for the allowed call-sites." >&2
    exit 1
fi

echo "Invariant band check passed."
