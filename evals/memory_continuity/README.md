# Agent Continuity evaluation

This suite measures the memory layer independently from the model that answers the final turn. It covers request-aware recall, procedure recall, active-skill routing, approval gates, sensitive-memory exclusion, correction/supersession, pinned context, token load, and retrieval latency. Version 2 also runs a physical partition test across two agents, two projects, two users, and the legacy store. It requires all intended personal/project/project-agent memories to be recalled and zero cross-boundary or legacy leakage.

Run Aura's implementation from the repository root:

```sh
cargo run -p aura-context-memory --example continuity_eval -- \
  --output target/memory-continuity-report.json
```

The report runs both `aura-query-aware-v1` and the prior `aura-salience-baseline`, then emits explicit recall, precision, token-load deltas, and a `scope_isolation` scorecard. The command exits non-zero if query-aware Aura misses any release guardrail: Recall@K below 90%, precision below 80%, any forbidden-memory recall, an average retrieval payload above 800 estimated tokens, or any physical scope-isolation failure.

## Compare Codex, Hermes, or another agent

Run the same `scenarios.json` through the other agent's memory adapter and save this neutral JSON shape:

```json
{
  "agent": "agent-name-and-version",
  "runs": [
    {
      "scenario_id": "recall-test-command",
      "recalled_ids": ["test-command"],
      "estimated_tokens": 42,
      "duration_ms": 8
    }
  ]
}
```

`recalled_ids` must contain only record IDs actually supplied to the answering model. This makes the comparison about memory selection rather than model eloquence. Include every scenario; missing runs score as zero recall. Then compare any number of agents in one report:

```sh
cargo run -p aura-context-memory --example continuity_eval -- \
  --comparison /path/to/codex-memory.json \
  --comparison /path/to/hermes-memory.json \
  --output target/memory-continuity-comparison.json
```

The checked-in `comparison.example.json` is a deliberately imperfect adapter fixture for validating the comparison path; it is not a claimed result for any real agent.
