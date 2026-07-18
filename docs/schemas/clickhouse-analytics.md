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
`export_version DateTime64(6, 'UTC')`. Timestamp-backed dimensions use the
exporter's observed `updated_at`; facts use export batch time. Sequence-backed
execution dimensions use a monotonic exporter version above the durable
`export_version_floor`, while the schema-upgrade backfill uses one fixed
`upgrade_version`. Readers must collapse duplicates with `FINAL` (dims and
facts only, never `events_raw`).

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

`dim_execution_runs` — `ORDER BY (tenant_id, run_uid)`:
`run_uid UUID, tenant_id UUID, contact_id Nullable(UUID), session_id UUID,
source_kind LowCardinality(String), route_reason LowCardinality(String),
skill_template_ref Nullable(String),
skill_template_revision_uid Nullable(UUID), initial_plan_hash String,
active_plan_hash String, plan_revision UInt64,
status LowCardinality(String), terminal_reason Nullable(String),
requirement_count UInt64, satisfied_requirement_count UInt64,
completion_check_count UInt64, logical_task_count UInt64,
reserved_cost_microusd UInt64, actual_cost_microusd UInt64,
reserved_tokens UInt64, actual_tokens UInt64,
reserved_tasks UInt64, actual_tasks UInt64,
reserved_tool_calls UInt64, actual_tool_calls UInt64,
reserved_retrieved_bytes UInt64, actual_retrieved_bytes UInt64,
queued_at Nullable(DateTime64(6,'UTC')),
started_at Nullable(DateTime64(6,'UTC')),
queue_to_start_ms Nullable(Float64),
completed_at Nullable(DateTime64(6,'UTC')), duration_ms Nullable(Float64),
created_at DateTime64(6,'UTC'), updated_at DateTime64(6,'UTC'),
export_version DateTime64(6,'UTC')`

`dim_execution_tasks` — `ORDER BY (tenant_id, run_uid, task_id)`:
`task_id UUID, run_uid UUID, tenant_id UUID, node_id String, item_key String,
task_kind LowCardinality(String), capability_name Nullable(String),
capability_version Nullable(String), plan_revision UInt64,
status LowCardinality(String), failure_class Nullable(String),
attempt UInt32, generation UInt64, citation_count UInt64,
reserved_cost_microusd UInt64, actual_cost_microusd UInt64,
reserved_tokens UInt64, actual_tokens UInt64,
reserved_tasks UInt64, actual_tasks UInt64,
reserved_tool_calls UInt64, actual_tool_calls UInt64,
reserved_retrieved_bytes UInt64, actual_retrieved_bytes UInt64,
queue_latency_ms Nullable(Float64), duration_ms Nullable(Float64),
started_at Nullable(DateTime64(6,'UTC')),
completed_at Nullable(DateTime64(6,'UTC')),
created_at DateTime64(6,'UTC'), updated_at DateTime64(6,'UTC'),
export_version DateTime64(6,'UTC')`

These tables match `analytics.execution_run_fact` and
`analytics.execution_task_fact` value-for-value. `session_id` and `item_key`
are non-null, and nullable skill/capability provenance remains nullable.
`source_ref`, `capability_ref`, `task_uid`, and raw `error` columns do not exist.
Raw input, output, gaps, cancellation reason, and error prose are never
exported. Execution-run analytics retain route reason/source; a constant
run-mode dimension is deliberately absent.

Run `queue_to_start_ms` is exactly `started_at - queued_at`; it is null when
either timestamp is null. Run `duration_ms` is `completed_at - started_at`.
Task `queue_latency_ms` is first `started_at - created_at`. Task `duration_ms`
uses `completed_at - started_at`, or `completed_at - created_at` for a task
terminalized before start. Durations are milliseconds and clamped at zero.

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
| execution_run_fact | `dim_execution_runs FINAL` |
| execution_task_fact | `dim_execution_tasks FINAL` |
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
4. **Postgres cursor indexes.** Timestamp-backed mutable tables have a plain
   `(updated_at)` btree index. Execution runs/tasks use the non-null
   `analytics_change_seq` plus primary UUID tuple and its supporting index;
   their exporter never scans or orders by `updated_at`.
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
    table_name          TEXT PRIMARY KEY,
    cursor_ts           TIMESTAMPTZ NOT NULL,
    cursor_id           UUID,
    cursor_seq          BIGINT,
    pass_high_water_seq BIGINT,
    pass_high_water_id  UUID,
    pass_started_at     TIMESTAMPTZ,
    exported_at         TIMESTAMPTZ NOT NULL,
    CHECK (
        (pass_high_water_seq IS NULL AND pass_high_water_id IS NULL
            AND pass_started_at IS NULL)
        OR
        (pass_high_water_seq IS NOT NULL AND pass_high_water_id IS NOT NULL
            AND pass_started_at IS NOT NULL)
    )
);
```

Timestamp-backed datasets keep `cursor_seq IS NULL` and continue using
`(cursor_ts, cursor_id)` with the overlap window. Sequence-backed execution
datasets use non-null `(cursor_seq, cursor_id)` and an exact zero sentinel of
`(0, 00000000-0000-0000-0000-000000000000)`. For those rows, `cursor_ts` is
the database time at which the exporter last durably reached the regular
sequence cursor; it is not a source `updated_at` watermark.

Each execution source row carries non-null `analytics_change_seq`. Every
run/task insert or analytics-relevant update takes
`pg_advisory_xact_lock_shared(1297047877, 337)`, then allocates the next
sequence, then writes the row. Upgrade and incremental high-water capture take
the matching exclusive transaction lock
`pg_advisory_xact_lock(1297047877, 337)`. This orders commits around the fence:
older shared writers are included, and writers queued behind the fence receive
a larger sequence for the next pass.

Under the single exporter lease, an incremental pass captures and persists the
greatest `(analytics_change_seq, primary_uuid)` tuple for each execution
dataset. Every page uses:

```sql
(analytics_change_seq, primary_uuid) > (cursor_seq, cursor_id)
AND (analytics_change_seq, primary_uuid)
    <= (pass_high_water_seq, pass_high_water_id)
ORDER BY analytics_change_seq, primary_uuid
```

If a pass bound exists after restart, the exporter resumes it without
recapturing. A page advances only after the idempotent
`ReplacingMergeTree` insert succeeds. Reaching the bound atomically advances
the regular cursor, clears the active pass, and sets `cursor_ts` and
`exported_at` to one database timestamp. An empty caught-up pass performs the
same timestamp update. Zero/reset and active or partial passes leave freshness
at the previous caught-up value.

`moa-edge` reports:

```sql
MIN(CASE WHEN cursor_seq IS NULL THEN cursor_ts ELSE exported_at END)
```

as `read_model_updated_at` across export-state rows.

## Execution Dimension Upgrade

`analytics.clickhouse_schema_upgrade_state`, keyed by
`execution_dimensions_v2`, is the durable upgrade state. Its checked stages
are:

```text
pending
  -> schema_upgraded
  -> cursors_reset
  -> runs_exported
  -> tasks_exported
  -> complete
```

The row persists non-null `upgrade_version` and `export_version_floor`, complete
run/task high-water sequence/UUID tuples, paired per-dataset page
sequence/UUID cursors, and stage timestamps. Stages move only forward.

Initialization captures both source high waters under the exclusive advisory
lock. Empty tables use the zero sentinel. `upgrade_version` is greater than
every existing execution-dimension version and becomes the initial
`export_version_floor`.

The upgrader then idempotently:

1. renames `task_uid` to `task_id`;
2. widens `plan_revision` from `UInt32` to `UInt64`;
3. repairs nullable/non-nullable columns;
4. adds every normalized execution fact field;
5. drops `source_ref`, `capability_ref`, and raw `error`;
6. validates the complete final column contract before recording
   `schema_upgraded`;
7. resets regular and upgrade page cursors to the zero sentinel;
8. exports runs and tasks through their fixed high-water tuples with the fixed
   `upgrade_version`;
9. marks `complete` only after both regular cursors equal their stored high
   waters and both caught-up timestamps are updated.

ClickHouse mutations run synchronously or the upgrader waits for
`system.mutations`. A restart resumes the durable stage and page cursor; it
does not repeat completed logical work. Normal incremental export remains
paused until the upgrade reaches `complete`. Each later page claims
`max(database_now, export_version_floor + 1 microsecond)` and persists the new
floor, so clock skew cannot resurrect upgrade or older rows.

Leader election remains the Postgres advisory lock
`pg_try_advisory_lock(hashtext('clickhouse-analytics-export'))` held for the
life of the loop; non-leaders sleep and retry.
