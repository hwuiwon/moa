# Analytics and Traces

Use this when the question is not just "did the turn finish," but also "do the numbers and timings agree with the durable record?"

## Source-Of-Truth Order

When these surfaces disagree, resolve them in this order:

1. persisted event log
2. current `sessions` row and generated columns
3. live views and refreshed materialized views
4. traces and runtime-event streams

This order matters because analytics and traces are derived surfaces.

## Analytics Checks

The analytics model is documented in:

- `docs/analytics.md`
- `crates/moa-core/src/analytics.rs` (or whichever current path holds the analytics types)
- `crates/moa-session/src/schema.rs`

Key invariants:

- generated columns own `total_input_tokens` and `cache_hit_rate`
- the `update_session_aggregates` trigger owns event-derived counters
- `session_summary` and `tool_call_summary` are live views
- `session_turn_metrics` and tenant daily metrics are materialized views and may be stale until refreshed. The tenant daily SQL view is `daily_storage_partition_metrics`.

If a metric looks wrong:

1. Confirm the expected underlying events exist.
2. Check the relevant `sessions` row counters.
3. Refresh materialized views before trusting cached rollups.
4. Compare the view output to the raw event arithmetic.

Typical questions:

- missing cost or token totals: was `BrainResponse` persisted with the expected payload?
- wrong cache hit rate: do the three input-token counters on `sessions` match the event log?
- tool success rate mismatch: does every `ToolCall` have the matching `ToolResult` or `ToolError`?
- daily stats stale: was `REFRESH MATERIALIZED VIEW CONCURRENTLY` run?

## Trace Checks

Turn-latency and replay guidance lives in:

- `docs/observability/turn-latency.md`
- `docs/11-event-replay-runbook.md`
- `crates/moa-brain/tests/brain_turn_cache_replay_db_memory.rs` (span structure + replay accounting)

The important span structure is:

```text
session_turn
├── pipeline_compile
├── llm_call
├── tool_dispatch
└── event_persist
```

Interpretation shortcuts:

- `pipeline_compile` high and growing: inspect replay and context build cost
- `llm_call` high: provider latency dominates
- `tool_dispatch` high: tools or approval/tool coordination dominate
- `event_persist` high: store writes, aggregate updates, or post-turn maintenance dominate

## Correlating Events To Spans

Ask these in order:

1. Did the expected event persist?
2. If yes, is the matching span missing or malformed?
3. If no, which last good span proves the turn stopped before persistence?
4. Did the analytics row reflect the persisted events after refresh?

Examples:

- `ToolResult` exists but no `BrainResponse`: focus on post-tool continuation logic, not provider latency
- `ApprovalRequested` exists and `ApprovalDecided` exists, but the orchestrator stays paused: focus on signal handling or wait conditions
- trace shows healthy `llm_call` and `tool_dispatch`, but session counts stay zero: focus on event persistence or aggregate-trigger execution

## Useful Commands

```bash
cargo test -p moa-session --tests
cargo test -p moa-brain --test brain_turn_cache_replay_db_memory -- --nocapture
```

Operational reads:

```bash
curl -X POST "$MOA_EDGE_URL/v1/analytics/session-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{"session_id":"<session-id>"}'
curl -X POST "$MOA_EDGE_URL/v1/analytics/tool-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{}'
curl -X POST "$MOA_EDGE_URL/v1/analytics/cache-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{"days":30}'
```
