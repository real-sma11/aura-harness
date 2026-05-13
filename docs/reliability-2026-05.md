# Harness reliability fixes — 2026-05

Coordinated companion to the `aura-os` agent-stream reliability work
(see `c:\code\aura-os` Phase 1–5 commits). This doc captures the
matching changes in `aura-harness` and the one remaining item that
must land in a separate repo before the reliability picture is fully
closed.

## What changed in `aura-reasoner`

### `AURA_LLM_EMERGENCY_BODY_CAP_BYTES` — proactive body trimming

The historical Phase-0 diagnostic cap (default `0` = disabled) has
been promoted into a permanent proactive request budget. The default
is now `524288` (512 KiB), generous enough for typical multi-turn
transcripts but well below the Cloudflare managed-router body-size
cliff that produced the 24 KiB cap incident.

Crucially, the trimming function (`fit_body_under_cap` in
`crates/aura-reasoner/src/anthropic/provider.rs`) now **always
succeeds**. Previously, when the cap was tight enough that truncating
the last user message alone couldn't get the body under the cap, the
function bailed with `truncated_ok: false` and the harness forwarded
the **un**-truncated body — straight into the Cloudflare WAF. That
was the exact symptom of debug session `95fd5c`: 488 `truncated_ok:
false` log lines followed by 22 Cloudflare `403` blocks in `aura-os`.

The new ladder:

1. **No-op.** Body already fits.
2. **Truncate last user.** Shrink the largest text / `tool_result`
   payload in the last user message.
3. **Drop oldest message pairs.** If the body is still over the cap
   (i.e. the bulk is in the *history*, not the latest turn), drop
   the oldest user/assistant pair (system messages are preserved)
   and re-attempt step 2. We deliberately drop in pairs to avoid
   stripping a `tool_use` without its matching `tool_result`.
4. **Collapse.** Last-ditch: replace `messages` with a single
   synthetic user turn carrying the truncation marker + a tail of
   the original user text. System messages survive.

`maybe_apply_emergency_body_cap` is gone; the new entry point is
`apply_body_cap`, but the env var name is preserved for backwards
compatibility with operator configs.

### `AURA_LLM_CLOUDFLARE_MAX_RETRIES` — 403 retry-with-shrink

`CLOUDFLARE_MAX_RETRIES` was a hard-coded `1`; the second 403
propagated as a terminal `Transient { status: 403, .. }` to
`aura-os`, where it surfaced as an `Error` SSE event and abruptly
ended the user's turn.

That const is now config-driven (`cloudflare_max_retries`, env
`AURA_LLM_CLOUDFLARE_MAX_RETRIES`, default `3`), and each successive
403 retry multiplies the effective body cap by `3/4`. Starting at
512 KiB the schedule is `384 / 288 / 216 / 162 / 121 KiB`; three
retries land at ~216 KiB, which is comfortably below every body-size
WAF rule we have observed on the managed router edge. There is a
floor of `16 KiB` below which we stop shrinking — past that point a
tighter cap only damages the conversation without appeasing any
known rule.

The shrink is plumbed through `run_model_chain_with_retries` via a
new `AttemptContext { model_idx, body_cap_override }` that the
attempt closure passes into `send_checked_with_cap`. Non-403 retry
paths (429/529 overload, 5xx) leave the cap alone — the explicit
`body_cap_override: None` in their `RetryAction::Retry` arms is
deliberate.

### Test coverage

New tests in `provider.rs` cover:

* `apply_body_cap_collapses_when_cap_is_tiny` — regression for the
  `truncated_ok: false` symptom that triggered debug session 95fd5c.
* `fit_body_under_cap_drops_oldest_pairs_when_history_is_the_bulk` —
  long-agent-loop case where the last turn is small but the
  transcript is huge.
* `fit_body_under_cap_collapses_when_single_user_message_too_big` —
  worst-case fallback.
* `classify_retry_action_shrinks_body_cap_on_cloudflare` —
  successive 403s converge on a smaller cap.
* `classify_retry_action_cloudflare_shrink_has_a_floor` — shrink
  bottoms out at 16 KiB.
* `effective_body_cap_honors_override_when_tighter` /
  `effective_body_cap_zero_disable_ignores_override` — operator's
  explicit `0` always dominates retries.

## Deferred — needs a follow-up PR in `aura-node`

Phase 2 of the `aura-os` reliability work added client-side auto-retry
for stream drops: when the harness WebSocket dies mid-turn,
`aura-os-server` reconnects to a fresh harness session and replays the
last user message behind a "Reconnecting…" banner.

That works around the symptom but does not preserve mid-turn LLM
state — the new session cold-starts. The proper fix is a
protocol-level `session_resume` on the swarm WebSocket so the harness
can hand back the in-flight stream from the original session, picking
up wherever the network failure occurred.

`session_resume` belongs in **`aura-node`** (the swarm gateway),
which is a separate repository, and was deliberately left out of this
PR to keep the harness changes focused. The required hooks on the
harness side are already in place:

* `aura-harness` opens one `SessionBridge` per chat turn; the bridge
  carries a stable `session_id` that `aura-node` could persist and
  hand back to a reconnecting client.
* `HarnessOutbound` events already include the `session_id`, so a
  resume request from `aura-os-server` can be matched without a new
  protocol frame.

When the `aura-node` change lands, the `aura-os` client-side
auto-retry can be downgraded from "spawn a fresh session and replay
the last message" to "issue a `session_resume` and continue
streaming", which closes the last visible mid-turn artefact for users
on a flaky link.
