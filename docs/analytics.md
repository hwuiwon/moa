# Analytics

MOA stores analytics in Postgres by default. Application code writes durable
product events and rows; triggers, generated columns, views, materialized
views, and `moa-analytics` own read models. When the optional top-level
`[clickhouse]` config section is present, dashboard read models are served
from ClickHouse instead (see "ClickHouse Backend" below); Postgres remains the
transactional source of truth either way.

## Write Path

Do not write derived session counters from application code.

- Generated columns on `sessions` own pure row-local derivations such as
  `total_input_tokens` and `cache_hit_rate`.
- The `update_session_aggregates` trigger owns event-derived counters such as
  turn count, event count, token totals, cost totals, checkpoint sequence, and
  cache-token splits.
- `task_segments` owns task-level outcome, skill/tool usage, turn count, and
  token cost.

## Legacy Views

Session and task internals still expose direct views used by runtime code:

- `session_summary`
- `tool_call_analytics`
- `tool_call_summary`
- `session_turn_metrics`
- `daily_storage_partition_metrics`
- `skill_resolution_rates`
- `segment_baselines`

`SkillInjector` uses `skill_resolution_rates` for skill ranking, and segment
assessment uses `segment_baselines` for structural comparison.

## Generic Analytics API

The public analytics API is:

- `GET /v1/analytics/catalog`
- `POST /v1/analytics/query`

Both routes require tenant operator authorization. `moa-analytics` owns the
allowlisted dataset catalog and query compiler; clients select datasets and
fields from the catalog instead of sending raw SQL.

Catalog datasets currently map to:

- `analytics.session_fact`
- `analytics.turn_fact`
- `analytics.tool_call_fact`
- `analytics.task_segment_fact`
- skill activations derived from `analytics.task_segment_fact`
- `analytics.procedure_run_fact`
- `analytics.procedure_node_run_fact`
- `analytics.learning_candidate_fact`
- `analytics.experiment_run_fact`
- `analytics.event_fact`

The compiler injects tenant predicates for every query. Materialized views are
tenant-keyed read models, not a replacement for authorization.

## Refresh Behavior

`PostgresSessionStore::refresh_analytics_materialized_views` refreshes the
session turn and daily storage-partition views plus the `analytics.*_fact`
materialized views. Refresh is cron-owned: the default
`analytics_materialized_views_refresh` cron job (schedule `0 */15 * * * *`)
invokes `SessionStore.refresh_analytics_materialized_views` every 15 minutes.
The refresh is single-flighted under a deployment-global Postgres advisory lock,
so overlapping cron ticks across replicas never herd concurrent refreshes, and
it records its last successful and failed run into
`analytics.materialized_view_refresh_state`.

`moa-edge` never triggers a refresh; it only reads
`materialized_view_refresh_state.last_success_at` to report read-model staleness
(`read_model_updated_at`) alongside query responses. Analytics queries always
proceed against the current materialized-view state.

Segment-specific views (`skill_resolution_rates` and `segment_baselines`) are
refreshed separately by session-store segment refresh helpers and by the
default `segment_materialized_views_refresh` cron job every 15 minutes.

## ClickHouse Backend

With `[clickhouse]` configured (`docs/schemas/clickhouse-analytics.md` is the
schema contract, `docs/plans/clickhouse-analytics-read-models.md` the design):

- A leader-leased exporter in `moa-orchestrator` (`analytics_export/`)
  incrementally copies dimension rows, the `events_raw` stream (with
  exporter-stamped `turn_number`), and the Postgres-computed windowed facts
  (`turn_fact`, `tool_call_fact`) into ClickHouse on a poll interval
  (`clickhouse.export_poll_secs`, default 15 s). Cursors live in
  `analytics.clickhouse_export_state`.
- `moa-analytics` compiles each catalog dataset to ClickHouse SQL
  (`AnalyticsBackend::ClickHouse`) and executes via
  `AnalyticsClickHouseClient`; `moa-edge` selects the backend from config and
  skips the matview refresh entirely. Response metadata
  `read_model_updated_at` reports the most-stale export cursor.
- Tenant isolation stays compiler-injected (`tenant_id = ?` bound first) on
  both backends. Tenant offboarding purges the ClickHouse copies
  (`AnalyticsClickHouseClient::purge_tenant`) after the relational purge.
- Runtime aggregates (`skill_resolution_rates`, `segment_baselines`,
  `task_strategy_success_rates`) and `analytics.scores` stay in Postgres.
- Validation: the certify skill's "ClickHouse Analytics Backend And Exporter"
  matrix — offline snapshots alone cannot catch ClickHouse syntax/semantic
  drift; the live parity lane is the gate.

## Adding Analytics

1. Prefer a regular view when live reads are cheap enough.
2. Prefer a materialized view when the query is expensive and stale reads are
   acceptable.
3. Add public dashboard fields through `moa-analytics` catalog metadata, not by
   exposing raw table names.
4. Keep tenant/storage-partition filters explicit in read models and compiled
   queries.
5. Store currency in integer cents.
