# Analytics

MOA stores session analytics in Postgres so application code does not maintain
its own rollups on the write path.

## Source of truth

- Generated columns on `sessions` own pure row-local derivations:
  - `total_input_tokens`
  - `cache_hit_rate`
- The `update_session_aggregates` trigger owns cross-event rollups on `sessions`:
  - `event_count`
  - `turn_count`
  - `total_input_tokens_uncached`
  - `total_input_tokens_cache_write`
  - `total_input_tokens_cache_read`
  - `total_output_tokens`
  - `total_cost_cents`
  - `last_checkpoint_seq`
- Views and materialized views own analytics queries:
  - `session_summary`
  - `tool_call_analytics`
  - `tool_call_summary`
  - `session_turn_metrics`
  - `daily_workspace_metrics`

Do not write session aggregate counters from application code. The trigger and
generated columns are the only supported write path for these values.

## Views

### `session_summary`

Live per-session rollup for CLI and operational reads.

Columns include:

- session identity and status
- `turn_count`
- `event_count`
- `total_input_tokens`
- `total_output_tokens`
- `total_cost_cents`
- `cache_hit_rate`
- derived `duration_seconds`
- `tool_call_count`
- `error_count`

### `tool_call_analytics`

Live per-call fact view over `ToolCall` plus matching `ToolResult` or
`ToolError` rows.

Columns include:

- workspace and session identity
- tool name and tool id
- call and finish timestamps
- `duration_ms`
- `success`

### `tool_call_summary`

Live per-tool aggregation built from `tool_call_analytics`.

Columns include:

- `call_count`
- `avg_duration_ms`
- `p50_ms`
- `p95_ms`
- `success_rate`

## Materialized views

### `session_turn_metrics`

Cached per-turn analytics derived from `BrainResponse` rows and the tool events
that occurred within each turn boundary.

Columns include:

- turn number
- model
- `llm_ms`
- `tool_ms`
- `tool_call_count`
- token counts
- `cost_cents`

`pipeline_ms` is present but currently `NULL` because turn-pipeline latency is
only traced, not yet persisted as an event payload.

### `daily_workspace_metrics`

Cached daily workspace rollup keyed by `(workspace_id, day)`.

Columns include:

- `session_count`
- `turn_count`
- `total_input_tokens`
- `total_cache_read_tokens`
- `total_output_tokens`
- `total_cost_cents`
- `avg_cache_hit_rate`

## Refresh behavior

`session_turn_metrics` and `daily_workspace_metrics` are refreshed with:

```sql
REFRESH MATERIALIZED VIEW CONCURRENTLY session_turn_metrics;
REFRESH MATERIALIZED VIEW CONCURRENTLY daily_workspace_metrics;
```

The local daemon runs this refresh loop hourly. CLI workspace and cache stats
also trigger an on-demand refresh before reading these materialized views.

## Adding new analytics

1. Prefer a regular view when live reads are cheap enough.
2. Prefer a materialized view only when the query is expensive and stale reads
   are acceptable.
3. Use generated columns only for pure expressions of columns in the same row.
4. Use triggers only for bounded mutations on the same session row.
5. Keep currency storage in integer cents.
