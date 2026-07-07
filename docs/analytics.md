# Analytics

MOA stores analytics in Postgres. Application code writes durable product
events and rows; triggers, generated columns, views, materialized views, and
`moa-analytics` own read models.

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
materialized views. `moa-edge` may trigger that refresh in the background before
analytics query reads, throttled to at most once per process per minute. The
query itself proceeds against the current materialized-view state.

Segment-specific views (`skill_resolution_rates` and `segment_baselines`) are
refreshed separately by session-store segment refresh helpers and by the
default `segment_materialized_views_refresh` cron job every 15 minutes.

## Adding Analytics

1. Prefer a regular view when live reads are cheap enough.
2. Prefer a materialized view when the query is expensive and stale reads are
   acceptable.
3. Add public dashboard fields through `moa-analytics` catalog metadata, not by
   exposing raw table names.
4. Keep tenant/storage-partition filters explicit in read models and compiled
   queries.
5. Store currency in integer cents.
