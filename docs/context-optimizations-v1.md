# Context Optimizations V1

This document explains, in simple terms, what changed in the harness, why it matters, and how to validate it.

## The Problem We Had Before

Before these changes, the harness could run turns, but it did not tell Aura OS the full truth about context and token usage.

In practice that meant:

- Aura OS mostly saw `input_tokens` and `output_tokens`.
- Cache write/read token data stopped inside the harness.
- `context_utilization` was too naive and under-reported real prompt occupancy.
- `provider` was blank.
- `files_changed` was empty even when the agent edited files.
- Compaction decisions were based on weaker signals than the ones the model was actually experiencing.
- If a prompt overflowed, the harness was more likely to fail hard instead of compacting and retrying cleanly.

## Old Flow Vs New Flow

### Before

```text
model usage
  -> partial session counters
  -> weak context estimate
  -> Aura OS sees only part of the picture
```

Example:

- The model may have been reusing a lot of prompt cache.
- The harness knew some of that internally.
- Aura OS still saw a much smaller and incomplete usage story.

### Now

```text
model usage
  -> per-turn usage with cache read/write tokens
  -> better context occupancy estimate
  -> provider + file change summary
  -> Aura OS stores and uses the richer signal
```

Example:

- A turn can now report:
  - billed input/output tokens
  - cache creation tokens
  - cache read tokens
  - estimated current context occupancy
  - provider name
  - changed files

That gives Aura OS enough truth to make better rollover, billing, analytics, and debugging decisions.

## What We Changed

### 1. Cache token plumbing

We now carry cache token information all the way through the stack:

- reasoner
- agent loop
- node session state
- protocol `SessionUsage`
- Aura OS ingestion

This was the biggest missing telemetry gap.

### 2. Better context accounting

We replaced the older “current input only” approximation with a better occupancy estimate.

That means `context_utilization` is now much closer to:

- what the prompt actually occupies,
- not just what the latest billed input field happened to say.

### 3. Provider and file-change reporting

The harness now reports:

- the actual provider
- net file changes across the turn

So downstream systems do not need to guess.

### 4. Safer compaction behavior

Compaction now lives in `aura-compaction`, which owns the pure policies and
mutations for message history, storage payloads, write inputs, cached tool
results, and tool surfaces. `aura-agent` decides when to invoke it and keeps the
model call for summary escalation outside the pure crate.

The implemented behavior is:

- reserve output headroom before the prompt is too full
- when overflow still happens, compact and retry instead of immediately failing
- gate write-input redaction on pressure, then replace bulky fields with
  structured `_redacted` metadata
- surface attempts to execute redacted write/edit payloads as
  `CompactionStructural` tool errors instead of writing placeholders to disk
- compact persisted session tool blobs with the same storage-facing API
- escalate to `SummaryInput` / `SummaryOutput` when local compaction is not
  enough; `aura-agent` performs the model-backed summary handoff

This is much closer to how strong production coding agents behave.

## Simple “Before / After” Examples

### Example 1: Token telemetry

Before:

- Aura OS might only see `input_tokens=112`

Now:

- Aura OS can see:
  - `input_tokens=112`
  - `cache_creation_input_tokens=128717`
  - `cache_read_input_tokens=107994`
  - `estimated_context_tokens=16774`
  - `context_utilization=0.08387`

Why this matters:

- Before, the system could think the session was tiny.
- Now, it can see the real prompt footprint and the real cache behavior.

### Example 2: File tracking

Before:

- A turn could edit files and still report no file changes.

Now:

- The same turn can report net `modified` files directly in `assistant_message_end`.

### Example 3: Overflow handling

Before:

- A prompt overflow was more likely to surface as a hard error.

Now:

- The harness can compact context and retry with a smaller response budget first.

## Real Benchmark Snapshot

We added a direct harness benchmark so we can test the harness without depending on the full Aura OS task pipeline.

Current branch 4-turn harness benchmark snapshot:

- model: `claude-opus-4-6`
- provider: `anthropic`
- turns: `4`
- billed input tokens: `92`
- billed output tokens: `5453`
- cache write tokens: `115217`
- cache read tokens: `87635`
- prompt footprint tokens: `202944`
- max estimated context tokens: `20085`
- max context utilization: `0.100425`
- file-change count: `4`
- estimated effective cost: `$0.9007`
- average time to first event: `2657.5 ms`

Important takeaway:

- Old-style telemetry would have made this look like roughly a `92` input-token session.
- The improved harness can now show that the session actually involved `202944` prompt-footprint tokens once cache activity is accounted for.

That is not just “more numbers.”
It is the difference between:

- guessing,
- and knowing what the model path actually experienced.

## Current Branch Vs `origin/main`

We also ran the exact same direct harness benchmark against:

- `origin/main`
- the current optimization branch

### `origin/main`

- billed input tokens: `98`
- billed output tokens: `5907`
- cache write tokens reported: `0`
- cache read tokens reported: `0`
- prompt footprint tokens reported: `98`
- max estimated context tokens reported: `0`
- provider: blank
- file-change count reported: `0`
- estimated effective cost: `$0.1482`
- quality pass: `false`

### Current branch

- billed input tokens: `92`
- billed output tokens: `5453`
- cache write tokens reported: `115217`
- cache read tokens reported: `87635`
- prompt footprint tokens reported: `202944`
- max estimated context tokens reported: `20085`
- provider: `anthropic`
- file-change count reported: `4`
- estimated effective cost: `$0.9007`
- quality pass: `true`

### What changed in practical terms

- The old harness did not expose cache activity at the protocol boundary.
- The old harness did not expose a meaningful context occupancy estimate.
- The old harness left provider blank.
- The old harness did not surface file changes in this benchmark.
- The current harness finished with a better quality outcome on this benchmark, but it did not reduce effective cost yet.

So the comparison is best understood as a **truthfulness and control improvement** more than a pure “token spend reduction” claim.

The important win is:

- `origin/main` makes the session look tiny and mostly opaque.
- the current branch shows the real cache-heavy prompt footprint and the real context growth.

That is the foundation we need before smarter rollover, compaction, and cost control can be trusted.

## What This V1 Does Well

- Makes token and cache telemetry truthful.
- Gives Aura OS better context fullness data.
- Makes file-change reporting useful.
- Makes overflow recovery safer.
- Centralizes context and storage compaction in `aura-compaction`.
- Gives us a direct benchmark path for harness validation.

## What Is Still V2

- broader semantic compaction beyond the current summary-escalation handoff
- richer provider-specific token counting before request submission
- more benchmark coverage across full Aura OS workflows
- automatic session rollover driven by the richer context signal

## Validation Commands

Current harness benchmark:

```bash
cd /Users/shahrozkhan/Documents/zero/aura-os
AURA_EVAL_VERBOSE=1 bash ./evals/local-stack/bin/run-harness-context-benchmark.sh
```

Compare current harness vs `origin/main` harness:

```bash
cd /Users/shahrozkhan/Documents/zero/aura-os
AURA_EVAL_VERBOSE=1 AURA_EVAL_RESULTS_DIR=test-results/current-harness bash ./evals/local-stack/bin/run-harness-context-benchmark.sh
AURA_EVAL_VERBOSE=1 AURA_EVAL_HARNESS_URL=http://127.0.0.1:3414 AURA_EVAL_RESULTS_DIR=test-results/baseline-harness bash ./evals/local-stack/bin/run-harness-context-benchmark.sh
cd interface
node ./scripts/compare-benchmark-usage.mjs \
  test-results/current-harness/aura-benchmark-usage-summary.json \
  test-results/baseline-harness/aura-benchmark-usage-summary.json \
  harness-context-benchmark-compare
```

Current API workflow benchmark:

```bash
cd /Users/shahrozkhan/Documents/zero/aura-os
AURA_EVAL_VERBOSE=1 ./evals/local-stack/bin/run-token-efficiency-api.sh "Hello world static site benchmark"
```

Note:

- The direct harness benchmark is currently the cleanest validation path for the harness improvements.
- The broader Aura OS benchmark still has a task-extraction bottleneck that is being debugged separately.
