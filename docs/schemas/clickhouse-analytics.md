# ClickHouse Analytics Schema Contract

_Source of truth for the tables shared by the analytics exporter
(`moa-orchestrator`) and the analytics query backend (`moa-analytics`).
See `docs/plans/clickhouse-analytics-read-models.md` for the design._

All tables live in the single configured database (`clickhouse.database`,
default `moa`) — no separate `dims` database; dimension tables use a `dim_`
prefix. All DDL is idempotent (`CREATE TABLE IF NOT EXISTS`) and bootstrapped
at exporter startup. `DateTime64(6, 'UTC')` everywhere; UUIDs as `UUID`;
JSON payloads as `String`.

Version columns: every `ReplacingMergeTree` table carries
`export_version DateTime64(6, 'UTC')` — the exporter's observed
`updated_at` (dims) or export batch time (facts). Readers must collapse
duplicates with `FINAL` (dims and facts only, never `events_raw`).

Tenant isolation: every table carries `tenant_id UUID`; the query compiler
injects `tenant_id = ?` on every query, mirroring the Postgres matview model.
The exporter stamps `tenant_id` by joining `sessions` where the source table
lacks it (`events`), and casts TEXT tenant ids (`task_segments`,
`learning_candidates`) to UUID, skipping rows that do not parse.

## events_raw (append stream)

```sql
CREATE TABLE IF NOT EXISTS <db>.events_raw (
    event_id UUID,
    session_id UUID,
    tenant_id UUID,
    storage_partition_id String,
    user_id String,
    sequence_num Int64,
    turn_number Int64,          -- stamped by exporter: 1 + count of BrainResponse
                                -- events with lower sequence_num in the session;
                                -- a BrainResponse event closes its own turn (its
                                -- turn_number counts itself)
    event_type LowCardinality(String),
    token_count Nullable(Int32),
    payload String,
    ts DateTime64(6, 'UTC')
) ENGINE = ReplacingMergeTree
PARTITION BY toYYYYMMDD(ts)
ORDER BY (tenant_id, session_id, sequence_num)
```

## Dimension tables (ReplacingMergeTree(export_version), queried with FINAL)

`dim_sessions` — `ORDER BY (tenant_id, session_id)`:
`session_id UUID, tenant_id UUID, storage_partition_id String, user_id String,
contact_id Nullable(UUID), status LowCardinality(String),
channel LowCardinality(String), model Nullable(String), title Nullable(String),
parent_session_id Nullable(UUID), total_input_tokens_uncached Int64,
total_input_tokens_cache_write Int64, total_input_tokens_cache_read Int64,
total_output_tokens Int64, total_cost_cents Int64, event_count Int64,
turn_count Int64, main_cost_cents Int64, auxiliary_cost_cents Int64,
created_at DateTime64(6,'UTC'), updated_at DateTime64(6,'UTC'),
completed_at Nullable(DateTime64(6,'UTC')),
export_version DateTime64(6,'UTC')`
(`main_cost_cents`/`auxiliary_cost_cents` are the `session_summary` tier cost
split; `completed_at` is `sessions.completed_at`.)

`dim_session_agent_context` — `ORDER BY (tenant_id, session_id)`:
`session_id UUID, tenant_id UUID, agent_id String, display_name Nullable(String),
agent_revision_uid Nullable(UUID), created_at DateTime64(6,'UTC'),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

`dim_task_segments` — `ORDER BY (tenant_id, session_id, segment_index)`:
`segment_id UUID, session_id UUID, tenant_id UUID, storage_partition_id String,
user_id String, segment_index Int32, task_summary Nullable(String),
outcome Nullable(String), assessment Nullable(String),
outcome_confidence Nullable(Float64), tools_used Array(String),
skills_activated Array(String), turn_count Int64, token_cost Int64,
started_at DateTime64(6,'UTC'), ended_at Nullable(DateTime64(6,'UTC')),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

`dim_artifact_run` — `ORDER BY (tenant_id, run_uid)`:
`run_uid UUID, tenant_id UUID, storage_partition_id String, user_id String,
session_id Nullable(UUID), revision_uid Nullable(UUID), procedure_ref String,
status LowCardinality(String), error Nullable(String),
started_at Nullable(DateTime64(6,'UTC')),
completed_at Nullable(DateTime64(6,'UTC')), created_at DateTime64(6,'UTC'),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

`dim_artifact_node_run` — `ORDER BY (tenant_id, run_uid, node_run_uid)`:
`node_run_uid UUID, run_uid UUID, tenant_id UUID, node_id String,
status LowCardinality(String), error Nullable(String),
started_at Nullable(DateTime64(6,'UTC')),
completed_at Nullable(DateTime64(6,'UTC')), created_at DateTime64(6,'UTC'),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

`dim_learning_candidates` — `ORDER BY (tenant_id, candidate_id)`:
`candidate_id UUID, tenant_id UUID, storage_partition_id String,
candidate_type LowCardinality(String), status LowCardinality(String),
target_id Nullable(String), target_label Nullable(String),
confidence Nullable(Float64),
risk_class Nullable(String), created_at DateTime64(6,'UTC'),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

`dim_experiment_run` — `ORDER BY (tenant_id, run_uid)`:
`run_uid UUID, tenant_id UUID, storage_partition_id String, name String,
target_kind LowCardinality(String), status LowCardinality(String),
score_run_id Nullable(UUID), session_id Nullable(UUID),
error Nullable(String), started_at Nullable(DateTime64(6,'UTC')),
completed_at Nullable(DateTime64(6,'UTC')), created_at DateTime64(6,'UTC'),
updated_at DateTime64(6,'UTC'), export_version DateTime64(6,'UTC')`

If a `V000325` fact view selects a source column that is missing above, the
implementer adds it to the dim (and to this document) rather than dropping the
dataset field.

## Windowed fact tables (computed in Postgres by the exporter)

`turn_fact` — `ENGINE ReplacingMergeTree(export_version)`,
`ORDER BY (tenant_id, session_id, turn_number)`:
`tenant_id UUID, storage_partition_id String, contact_id Nullable(UUID),
user_id String, session_id UUID, turn_number Int64,
finished_at DateTime64(6,'UTC'), model Nullable(String),
pipeline_ms Nullable(Float64), llm_ms Float64, tool_ms Float64,
tool_call_count Int64, input_tokens_uncached Int64,
input_tokens_cache_write Int64, input_tokens_cache_read Int64,
total_input_tokens Int64, output_tokens Int64, cost_cents Int64,
export_version DateTime64(6,'UTC')`

Row values must match `session_turn_metrics`
(`V000307__tenant_runtime_boundaries.sql:515`) row-for-row: same turn
numbering (ROW_NUMBER over BrainResponse), same tool window
(`prev_response_seq < tool_call_seq < response_seq`), same first-match
ToolResult/ToolError duration fallback.

`tool_call_fact` — `ENGINE ReplacingMergeTree(export_version)`,
`ORDER BY (tenant_id, session_id, call_sequence_num)`:
`tenant_id UUID, storage_partition_id String, user_id String, session_id UUID,
call_sequence_num Int64, turn_number Int64, tool_id Nullable(UUID),
tool_name String, success Nullable(Bool), duration_ms Nullable(Float64),
model_tier Nullable(String), ts DateTime64(6,'UTC'),
export_version DateTime64(6,'UTC')`

Row values must match the effective `tool_call_analytics` view logic
(`V000001__session_baseline.sql:624`, from `V000008`, which adds the
constant `model_tier = 'main'`). `turn_number` is not a column of that view;
the exporter stamps each tool call with its enclosing turn
(`1 + count of earlier BrainResponses`, the same prefix function used for
`events_raw`). `tool_id` is `(call_data ->> 'tool_id')::UUID`.

## Dataset mapping (moa-analytics CH backend)

| Catalog dataset | ClickHouse source |
|---|---|
| session_fact | `dim_sessions FINAL` ⋈ `dim_session_agent_context FINAL`, tool/error counts aggregated from `events_raw` at query time as `uniqExactIf(event_id, event_type = 'ToolCall')` / `uniqExactIf(event_id, event_type = 'Error')` grouped by session (duplicate-tolerant, since `events_raw` is read without `FINAL`) |
| turn_fact | `turn_fact FINAL` ⋈ `dim_session_agent_context FINAL` |
| tool_call_fact | `tool_call_fact FINAL` ⋈ `dim_session_agent_context FINAL` |
| event_fact | `events_raw` ⋈ `dim_session_agent_context FINAL` (no FINAL on events_raw) |
| task_segment_fact | `dim_task_segments FINAL` ⋈ `dim_session_agent_context FINAL` |
| procedure_run_fact | `dim_artifact_run FINAL` ⋈ `dim_session_agent_context FINAL` |
| procedure_node_run_fact | `dim_artifact_node_run FINAL` ⋈ `dim_artifact_run FINAL` |
| learning_candidate_fact | `dim_learning_candidates FINAL` |
| experiment_run_fact | `dim_experiment_run FINAL` |

No ClickHouse materialized views in v1 — session rollups are cheap at query
time in CH; MVs come later only if profiling demands (and any MV over
`events_raw` must use duplicate-tolerant states, see below).

## Performance and correctness rules (binding)

1. **Duplicate visibility on `events_raw`.** ReplacingMergeTree collapses
   duplicates at merge time, not insert time, and the exporter's overlap
   window re-inserts rows on purpose. Therefore:
   - Aggregates over `events_raw` must be duplicate-tolerant:
     `uniqExact(event_id)` / `uniqExactIf(event_id, …)` for counts, `argMax`
     for latest-value reads — never raw `count()`/`sum()`/`countIf()`.
   - Row listings over `events_raw` (event_fact dataset) must dedup with
     `LIMIT 1 BY (session_id, sequence_num)`.
   - dims and fact tables are exempt: they are read with `FINAL`.
2. **`FINAL` only behind a primary-key tenant filter.** Every dim/fact query
   filters `tenant_id = ?` (the pk prefix) before `FINAL`, so merge-on-read
   touches one tenant's range, not the table.
3. **Percentiles** use `quantileExactInclusive` (matches `PERCENTILE_CONT`
   for parity); revisit approximate `quantile` only if profiling shows
   memory pressure.
4. **Postgres cursor indexes.** Every exported mutable table must have a
   plain `(updated_at)` btree index (added in the exporter migration where
   missing) so the 15-second cursor pull is an index range scan, never a
   sequential scan on the OLTP database.
5. **Turn stamping is a full-prefix computation.** `turn_number` for an
   exported event counts BrainResponse events over the session's entire
   prefix (indexed per-session lookup over `(session_id, …)`), never a
   window over the current export batch alone.
6. **Compiled limits always apply.** The CH dialect keeps the compiler's
   row-limit clamp on every query, same as Postgres.
7. **Client reuse.** The ClickHouse `Client` is constructed once and reused
   (AppState / long-lived service), never per request.

## Export cursor state (Postgres)

```sql
CREATE TABLE analytics.clickhouse_export_state (
    table_name  TEXT PRIMARY KEY,
    cursor_ts   TIMESTAMPTZ NOT NULL,
    cursor_id   UUID,
    exported_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Overlap window: cursor rewinds `2 × export_poll_secs` on each poll; duplicate
rows are absorbed by ReplacingMergeTree keys. Leader election: Postgres
advisory lock (`pg_try_advisory_lock(hashtext('clickhouse-analytics-export'))`)
held for the life of the loop; non-leaders sleep and retry.
