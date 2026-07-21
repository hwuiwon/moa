# moa-analytics-export

Incremental Postgres-to-ClickHouse analytics exporter. When `[clickhouse]` is
configured, the export loop pulls changed rows from Postgres and lands derived
analytics copies in ClickHouse: the `events_raw` append stream, seven `dim_*`
dimension tables, and the two windowed fact tables (`turn_fact`,
`tool_call_fact`). Postgres stays the transactional source of truth;
ClickHouse holds analytical copies only.

The exporter is a plain background tokio task spawned by the orchestrator
binary via `spawn_analytics_export` — not a Restate service. Only one pod
exports at a time: leadership is a Postgres session advisory lock held on a
dedicated connection for the life of the loop, and cursors persist in
`analytics.clickhouse_export_state` with an overlap rewind that ClickHouse
`ReplacingMergeTree` keys absorb.

## Modules

- `lib.rs` — `AnalyticsExporter` loop, leader lease, cursors, and
  `spawn_analytics_export`.
- `events` — incremental `(timestamp, id)`-cursored pull of `events` into the
  `events_raw` append stream.
- `dims` — dimension-table export with ReplacingMergeTree upserts and
  monotonic export versions.
- `facts` — windowed `turn_fact` / `tool_call_fact` export, computed in
  Postgres by reusing the `session_turn_metrics` / `tool_call_analytics` SQL.
- `schema` — idempotent ClickHouse DDL bootstrap plus an exact contract check.

See `docs/plans/clickhouse-analytics-read-models.md` and
`docs/schemas/clickhouse-analytics.md` for the table contract.
