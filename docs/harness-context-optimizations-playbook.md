# Harness Context Optimizations Playbook

This document is the simple, shareable explanation of the harness context work.

It answers four questions:

1. What was wrong before?
2. What did we change?
3. How do we test it?
4. What did the live benchmark actually prove?

## Current Status

What is solid right now:

- richer cache-aware usage reporting is implemented end to end
- Aura OS now consumes and persists the richer usage correctly
- context occupancy, overflow recovery, and file-change reporting are improved
- `aura-compaction` is the single owner for pure compaction behavior
- clean same-system cache-on vs cache-off benchmarks now show real wins on the
  scenarios we trust most
- the live Aura OS API benchmark lane is working again for full lifecycle
  validation of org -> agent -> project -> spec -> task -> build flows

What is still more exploratory:

- broader live benchmark coverage across many more task shapes
- more aggressive tool-output shaping beyond repeated cached reads
- semantic compaction and longer-horizon session rollover work
- a fully consistent live automaton/session telemetry story across every API
  benchmark scenario

## Why This Work Matters

For a coding agent, harness quality is not just a performance concern.
It directly affects:

- cost
- latency
- context-limit behavior
- session reliability
- how much the rest of the system can trust usage and billing signals

If the harness reports incomplete usage, compacts too late, or fails badly near
context limits, the whole agent stack becomes harder to reason about.

## The Problem Before

Before this work, the harness could execute turns, but the surrounding system
did not see the full truth about what the model path was experiencing.

Main gaps:

- cache write/read usage stopped inside the harness
- `context_utilization` under-reported real prompt occupancy
- `provider` was blank in session usage payloads
- `files_changed` was missing or incomplete
- compaction triggers relied on weaker token signals
- prompt overflow handling was less graceful

In practice, that meant a long-running session could look much smaller and much
safer than it really was.

## What We Changed

### 1. Cache-aware usage reporting

We now carry cache token data all the way through:

```text
provider response
  -> reasoner usage
  -> agent loop result
  -> node session state
  -> protocol SessionUsage
  -> Aura OS ingestion
```

That means downstream systems can now see:

- billed input tokens
- billed output tokens
- cache creation input tokens
- cache read input tokens

### 2. Better context occupancy estimation

We replaced the old "current input only" approximation with a better estimate
of actual prompt occupancy.

That gives us a more useful:

- `estimated_context_tokens`
- `context_utilization`

This matters because compaction, rollover, and debugging decisions should be
based on prompt pressure, not just raw billed input.

### 3. Better protocol truthfulness

The harness now reports:

- real provider name
- net file changes for the turn

So Aura OS does not have to reconstruct or guess important session facts.

### 4. Safer compaction and overflow recovery

Compaction is centralized in `aura-compaction`: message-history tiers,
pressure-gated write-input redaction, cached-result shaping, tool-surface
compaction, and storage compaction all live in one pure crate. The agent loop
passes context pressure into that crate and keeps model-backed summary
escalation in `aura-agent`.

The important runtime behavior is:

- reserve output headroom before the context window is too full
- when overflow still happens, compact and retry instead of failing immediately
- replace bulky write/edit inputs with structured `_redacted` markers only once
  pressure warrants it
- reject accidental execution of redacted write/edit payloads as
  `CompactionStructural`
- request a summary escalation handoff when local compaction cannot hit the
  target size

This gives the harness a much stronger recovery path for long sessions.

### 5. Repeated cached-read shaping

Large repeated cache hits for read-only tools are now compacted before they are
re-inserted into the prompt on later turns.

That keeps repeated read-heavy workflows from bloating the prompt unnecessarily.

## Simple Before / After

### Before

```text
model usage
  -> partial counters
  -> weak context estimate
  -> incomplete session telemetry
```

Example:

- a session may have reused a large prompt prefix
- the harness knew some of that internally
- Aura OS still only saw a small billed-input number

### After

```text
model usage
  -> cache-aware per-turn usage
  -> context occupancy estimate
  -> provider + file changes
  -> safer compaction / retry behavior
  -> truthful downstream telemetry
```

## Live Example

We ran a clean local A/B on the same 4-turn static-site scenario using the same
stack, same prompts, and same model path.

The only difference was:

- cache on
- cache off

These numbers are from the clean, post-merge static-site benchmark baseline we
froze for repeatable comparison.

### Cache On

- success: yes
- quality pass: yes
- input tokens: `27`
- output tokens: `4362`
- cache write tokens: `11795`
- cache read tokens: `84311`
- prompt input footprint: `96133`
- max context utilization: `0.0579`
- estimated cost: `$0.225058`
- total runtime: `96154 ms`

### Cache Off

- success: yes
- quality pass: yes
- input tokens: `258232`
- output tokens: `5844`
- cache write tokens: `0`
- cache read tokens: `0`
- prompt input footprint: `258232`
- max context utilization: `0.16714`
- estimated cost: `$1.43726`
- total runtime: `145918 ms`

### Delta: Cache On vs Cache Off

- input tokens: `-258205`
- output tokens: `-1482`
- estimated cost: `-$1.212202`
- runtime: `-49764 ms`
- prompt footprint: `-162099`
- max context utilization: `-0.1092`

## What This Result Means

For this clean benchmark run, the optimized harness with caching enabled was:

- cheaper
- faster
- less context-heavy
- still successful
- still quality-passing

This is important because it is not just a telemetry win.
On this scenario, it produced a real product win.

We also validated the same pattern on a second clean node-server patch
scenario. That matters because it makes this less likely to be a one-off lucky
run.

## What We Validate

We do not rely on one giant benchmark alone.
The validation strategy is layered.

### Layer 1: Unit and component tests

We validate the local harness logic directly:

- cache token propagation
- context estimation
- file-change tracking
- overflow retry behavior
- repeated cached-read shaping
- structured redaction markers and storage compaction
- workspace-root handling

### Layer 2: Fixture-backed harness evals

We run direct harness scenarios with explicit validators to catch:

- workspace visibility bugs
- bad relative-path handling
- artifact correctness problems

### Layer 3: Live A/B harness benchmarks

We run the same scenario with:

- caching enabled
- caching disabled

This gives us a fair same-system comparison for:

- cost
- speed
- context pressure
- quality

### Layer 4: Live Aura OS API benchmarks

We also run the real Aura lifecycle through the local API benchmark lane:

- org resolution or creation
- agent creation
- project import
- spec generation
- task extraction
- autonomous build loop execution
- artifact verification

This is not the best lane for fine-grained harness telemetry yet.
It is the best lane for validating that the real product loop still works after
we change the harness.

Current honest read:

- this lane is valuable and should stay in the test strategy
- it already catches real product regressions and brittle eval assertions
- some scenarios still surface richer session telemetry more reliably than
  others, so direct harness benchmarks remain the source of truth for the
  cache/context numbers we use in merge decisions

## Merge-Readiness Notes

Why this is mergeable as V1:

- the runtime changes are covered by deterministic Rust tests
- Aura OS wiring is covered by server and session tests
- benchmark pricing now fails loudly when pricing is unknown
- clean benchmark baselines are frozen for post-merge verification

What we should still keep doing after merge:

- restart the live harness before final benchmark passes to avoid stale-process noise
- keep validator-backed fixture scenarios as regression gates
- add more cross-tool and longer-horizon scenarios in the next phase

## Benchmarking Rules We Should Keep

These are the rules that make the benchmark trustworthy:

1. Compare the same harness against itself when possible.
2. Keep model, prompts, and stack constant.
3. Prefer clean worktrees for final benchmark runs.
4. Treat quality as a gate, not a side note.
5. Do not claim cost wins from incomplete pricing or incomplete token data.
6. Use fixture validators for correctness, not just text summaries.

## How To Re-run

From `aura-os`:

```bash
cd /Users/shahrozkhan/Documents/zero/aura-os

# Source the local stack auth token if running the Node benchmark directly.
set -a
source ./evals/local-stack/.runtime/auth.env
set +a

# Cache-on run
cd interface
AURA_EVAL_RESULTS_DIR=test-results/clean-post-merge-cache-on \
  AURA_EVAL_SCENARIO_ID=harness-context-static-site \
  node ./scripts/run-harness-context-benchmark.mjs
```

For the paired A/B flow, use the wrapper:

```bash
cd /Users/shahrozkhan/Documents/zero/aura-os
./evals/local-stack/bin/run-harness-context-cache-ab.sh
```

## Current V1 Conclusion

This optimization pass is a strong V1.

What is done:

- truthful cache-aware usage reporting
- better context occupancy tracking
- provider and file-change reporting
- safer compaction and overflow recovery
- repeated cached-read shaping
- unified `aura-compaction` ownership plus summary escalation handoff
- repeatable benchmark and fixture validation paths

What is still V2:

- broader live benchmark coverage
- more semantic compaction
- more tool-output shaping
- longer-horizon regression suites

## The Short Version

Before:

- the harness ran
- but the system did not see the full truth
- and long-context behavior was weaker

Now:

- the harness reports the truth much better
- the system can reason about context and cache behavior properly
- the loop handles overflow more safely
- and on a clean live benchmark, caching produced a real cost and runtime win
