# Step 92 — Generated Columns and Analytic Views Replace App-Side Aggregation

_Move per-session rollups (cache_hit_rate, cost, turn counts, tool success rates, latency percentiles) from application code into Postgres generated columns and views. Application code becomes thinner; the numbers stay always-consistent; analytics queries become SQL instead of ad-hoc aggregation._

---

## 1. What this step is about

Today MOA computes rollups on the write path:

- Step 79 accumulates `total_input_tokens_uncached / cache_write / cache_read / output` on `SessionMeta` via per-turn `UPDATE` statements.
- Step 81's turn-latency decomposition logs are aggregated client-side for any dashboards.
- Cost is computed in application code from token counts × pricing table.
- Per-tool success rate, per-tool mean latency, loop-trigger rates — all ad-hoc queries against the event log, re-implemented wherever needed.

Two problems:

1. **Drift risk.** An UPDATE that misses the rollup, a partial failure, a bug in the computation — all cause the stored aggregate to disagree with the underlying events. Worse, we have no detection for this.
2. **Duplication.** Every new analytic panel reimplements its own aggregation.

Postgres fixes both with two mechanisms:

- **Generated columns** compute derived values from base columns automatically. Writing cache_read + uncached implicitly updates cache_hit_rate — no application code, no drift.
- **Views and materialized views** centralize analytic queries. A dashboard, a CLI report, a metrics exporter all `SELECT` from the same view.

This step replaces ~300 lines of application aggregation code with ~100 lines of SQL.

---

## 2. Files to read

- `moa-session/src/schema.rs` (renamed in step 83) — where `sessions` and `events` tables are declared.
- `moa-session/src/queries.rs` — every `UPDATE sessions SET total_... = ...` is a candidate for deletion.
- `moa-core/src/types/session.rs` — `SessionMeta` struct.
- `moa-orchestrator/src/local.rs` — where per-turn aggregates are incremented.
- Step 79's `TokenUsage` implementation.
- Postgres docs: `GENERATED ALWAYS AS ... STORED`, views, materialized views, `REFRESH MATERIALIZED VIEW CONCURRENTLY`.

---

## 3. Goal

1. The `sessions` table carries base counters (per-turn sums of uncached / cache_read / cache_write / output / cost_cents); `cache_hit_rate` and `total_input_tokens` become `GENERATED ALWAYS AS ... STORED` generated columns. Application code stops computing these.
2. A new `session_turn_metrics` materialized view exposes per-turn metrics (pipeline_ms, llm_ms, tool_ms, tokens, cost) joined across sessions for analytics.
3. A `tool_call_analytics` view exposes per-tool success rate, p50/p95 latency, and call count, aggregated over the event log.
4. The orchestrator's per-turn `UPDATE sessions SET total_input_tokens_cache_read = ...` calls are deleted. Events are inserted; counters update automatically via a `GENERATED` column OR a dedicated trigger. Pick one; recommend triggers (cheaper than `GENERATED` on mutable base tables, and we need a few aggregate counters that span events).
5. CLI: `moa session stats <id>` becomes a one-query read from the views.

---

## 4. Rules

- **Generated columns are for pure functions of other columns in the same row.** Use them for `cache_hit_rate = cache_read / (cache_read + uncached + cache_write)`. Don't use them for cross-row aggregations.
- **Triggers update cross-event aggregates.** A single `AFTER INSERT` trigger on `events` updates the session row's counters. One place for the logic; no application-side duplication.
- **Views over tables for analytics.** Views reflect live data; materialized views cache. Use materialized views only when the query is slow AND the data is tolerant of staleness. Per-session stats → view. Cross-workspace daily rollups → materialized view refreshed hourly.
- **Migrations preserve existing data.** `ALTER TABLE sessions ADD COLUMN ... GENERATED ...` recomputes on rewrite. For existing rows, an `UPDATE ... SET ... = ...` is needed after adding the column (Postgres supports this for stored generated columns).
- **Don't reinvent the money column.** Use `INTEGER cost_cents` everywhere. Never `FLOAT` for currency. Derived columns can return fractional USD for display; storage stays integer.
- **Trigger side effects stay local.** Triggers update only the same session's row. No fan-out, no cascading mutations. That keeps the write latency bounded.

---

## 5. Tasks

### 5a. Schema: generated columns on `sessions`

Migration `moa-session/migrations/00N_session_generated_columns.sql`:

```sql
-- Add generated total_input_tokens (sum of three input counters)
ALTER TABLE sessions
    ADD COLUMN total_input_tokens BIGINT GENERATED ALWAYS AS (
        COALESCE(total_input_tokens_uncached, 0)
        + COALESCE(total_input_tokens_cache_write, 0)
        + COALESCE(total_input_tokens_cache_read, 0)
    ) STORED;

-- Cache hit rate as a stored computed column (0.0 to 1.0)
ALTER TABLE sessions
    ADD COLUMN cache_hit_rate DOUBLE PRECISION GENERATED ALWAYS AS (
        CASE WHEN (
            COALESCE(total_input_tokens_uncached, 0)
            + COALESCE(total_input_tokens_cache_write, 0)
            + COALESCE(total_input_tokens_cache_read, 0)
        ) = 0 THEN 0.0
        ELSE (COALESCE(total_input_tokens_cache_read, 0))::DOUBLE PRECISION
           / (COALESCE(total_input_tokens_uncached, 0)
              + COALESCE(total_input_tokens_cache_write, 0)
              + COALESCE(total_input_tokens_cache_read, 0))::DOUBLE PRECISION
        END
    ) STORED;

-- Index to pull "expensive sessions" or "low cache hit rate sessions" quickly
CREATE INDEX sessions_cache_hit_rate ON sessions (cache_hit_rate);
CREATE INDEX sessions_cost_cents ON sessions (total_cost_cents DESC);
```

### 5b. Trigger: update base counters from event inserts

```sql
CREATE OR REPLACE FUNCTION update_session_aggregates() RETURNS TRIGGER AS $$
BEGIN
    IF NEW.event_type = 'BrainResponse' THEN
        UPDATE sessions SET
            event_count = event_count + 1,
            total_input_tokens_uncached  = total_input_tokens_uncached  + COALESCE((NEW.payload->>'input_tokens_uncached')::INT, 0),
            total_input_tokens_cache_read  = total_input_tokens_cache_read  + COALESCE((NEW.payload->>'input_tokens_cache_read')::INT, 0),
            total_input_tokens_cache_write = total_input_tokens_cache_write + COALESCE((NEW.payload->>'input_tokens_cache_write')::INT, 0),
            total_output_tokens = total_output_tokens + COALESCE((NEW.payload->>'output_tokens')::INT, 0),
            total_cost_cents = total_cost_cents + COALESCE((NEW.payload->>'cost_cents')::INT, 0),
            turn_count = turn_count + 1,
            updated_at = NEW.timestamp
        WHERE id = NEW.session_id;
    ELSE
        UPDATE sessions SET
            event_count = event_count + 1,
            updated_at = NEW.timestamp
        WHERE id = NEW.session_id;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER trg_update_session_aggregates
    AFTER INSERT ON events
    FOR EACH ROW
    EXECUTE FUNCTION update_session_aggregates();
```

Then delete all the application-side `UPDATE sessions SET total_...` statements. Confirm by grep: `grep -rn 'UPDATE sessions SET total' moa-session/` should return zero matches.

### 5c. Views for analytics

`moa-session/migrations/00N_analytic_views.sql`:

```sql
-- Per-turn tool latency / success, derived from ToolCall + ToolResult events.
CREATE OR REPLACE VIEW tool_call_analytics AS
SELECT
    call.session_id,
    (call.payload->>'tool_name') AS tool_name,
    (call.payload->>'tool_id')::UUID AS tool_id,
    call.timestamp AS called_at,
    result.timestamp AS finished_at,
    EXTRACT(EPOCH FROM (result.timestamp - call.timestamp)) * 1000 AS duration_ms,
    COALESCE((result.payload->>'success')::BOOLEAN, FALSE) AS success
FROM events call
LEFT JOIN events result
    ON result.session_id = call.session_id
    AND result.event_type = 'ToolResult'
    AND (result.payload->>'tool_id')::UUID = (call.payload->>'tool_id')::UUID
WHERE call.event_type = 'ToolCall';

CREATE OR REPLACE VIEW tool_call_summary AS
SELECT
    tool_name,
    COUNT(*) AS call_count,
    AVG(duration_ms) AS avg_duration_ms,
    PERCENTILE_CONT(0.5) WITHIN GROUP (ORDER BY duration_ms) AS p50_ms,
    PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY duration_ms) AS p95_ms,
    SUM(CASE WHEN success THEN 1 ELSE 0 END)::DOUBLE PRECISION / NULLIF(COUNT(*), 0) AS success_rate
FROM tool_call_analytics
WHERE finished_at IS NOT NULL
GROUP BY tool_name;

-- Per-session summary: the things a user asks about
CREATE OR REPLACE VIEW session_summary AS
SELECT
    s.id,
    s.workspace_id,
    s.user_id,
    s.status,
    s.turn_count,
    s.event_count,
    s.total_input_tokens,
    s.total_output_tokens,
    s.total_cost_cents,
    s.cache_hit_rate,
    s.created_at,
    s.updated_at,
    EXTRACT(EPOCH FROM (s.updated_at - s.created_at)) AS duration_seconds,
    (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id AND e.event_type = 'ToolCall') AS tool_call_count,
    (SELECT COUNT(*) FROM events e WHERE e.session_id = s.id AND e.event_type = 'Error') AS error_count
FROM sessions s;
```

These views are recomputed on read. For typical session counts (thousands) they're fast enough.

### 5d. Materialized view for cross-session daily rollups

When someone asks "how much did caching save us this month", we don't want to scan all events. A materialized view caches the aggregate:

```sql
CREATE MATERIALIZED VIEW daily_workspace_metrics AS
SELECT
    workspace_id,
    DATE_TRUNC('day', created_at) AS day,
    COUNT(*) AS session_count,
    SUM(turn_count) AS turn_count,
    SUM(total_input_tokens) AS total_input_tokens,
    SUM(total_input_tokens_cache_read) AS total_cache_read_tokens,
    SUM(total_output_tokens) AS total_output_tokens,
    SUM(total_cost_cents) AS total_cost_cents,
    AVG(cache_hit_rate) AS avg_cache_hit_rate
FROM sessions
GROUP BY workspace_id, DATE_TRUNC('day', created_at);

CREATE UNIQUE INDEX daily_workspace_metrics_pk
    ON daily_workspace_metrics (workspace_id, day);
```

Refresh via `REFRESH MATERIALIZED VIEW CONCURRENTLY daily_workspace_metrics;` on a schedule — either from a tokio task every hour or via `pg_cron` if installed.

### 5e. Rust-side query helpers

Create a small analytics module in `moa-core/src/analytics.rs`:

```rust
pub struct SessionSummary {
    pub id: SessionId,
    pub turn_count: i32,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_cost_cents: i32,
    pub cache_hit_rate: f64,
    pub duration_seconds: f64,
    pub tool_call_count: i64,
    pub error_count: i64,
}

pub async fn get_session_summary(pool: &PgPool, id: &SessionId) -> Result<SessionSummary> {
    sqlx::query_as::<_, SessionSummary>(
        "SELECT id, turn_count, total_input_tokens, total_output_tokens,
                total_cost_cents, cache_hit_rate, duration_seconds,
                tool_call_count, error_count
         FROM session_summary WHERE id = $1"
    )
    .bind(id.to_string())
    .fetch_one(pool).await.map_err(...)
}
```

`moa session stats <id>` now does one SELECT. No application-side math.

### 5f. CLI surface

```
moa session stats <id>          # single session summary
moa workspace stats              # current workspace summary over last 30 days
moa tool stats                   # per-tool latency/success across all sessions
moa cache stats                  # cache hit rate trend, savings estimate
```

Each is a one-query read from a view.

### 5g. Delete the old code

After the trigger is live and verified:
- Delete application-side `UPDATE sessions SET total_...` in `emit_event`.
- Delete in-app summing of `TokenUsage` onto `SessionMeta` in the orchestrator turn loop.
- Delete per-tool aggregation helpers that duplicated what `tool_call_summary` now provides.

Grep to verify zero remnants:

```
grep -rn 'UPDATE sessions SET total' moa-session/
grep -rn 'session.total_input_tokens +=' moa-orchestrator/ moa-brain/
grep -rn 'cache_hit_rate =' moa-orchestrator/ moa-brain/ moa-core/  # only computed in SQL now
```

### 5h. Tests

- Integration: insert 3 `BrainResponse` events with known token counts into a session. Read back `sessions.total_input_tokens_cache_read` and `cache_hit_rate`. Values must match the arithmetic.
- Integration: a trigger error rolls back the event insert. Verify via a `CHECK` constraint violation synthesized in the trigger (or injected faulty payload).
- Integration: `tool_call_summary` view returns correct p50/p95/success_rate over seeded ToolCall + ToolResult events.
- Integration: after `REFRESH MATERIALIZED VIEW CONCURRENTLY`, `daily_workspace_metrics` reflects the underlying sessions.
- Correctness: after this step lands, run the step 78 integration test. Then query `session_summary` for the test session and verify the numbers against what the old application-side logic computed (reference implementation kept behind a `#[cfg(test)]` helper for one release).

### 5i. Migration of existing data

Backfill generated columns and re-run the trigger's effects for pre-existing events:

```sql
-- After adding the generated columns, existing rows are auto-populated by Postgres.
-- Nothing to backfill for generated columns.

-- For the trigger to apply retroactively, re-run its body manually:
UPDATE sessions s SET
    total_input_tokens_uncached = COALESCE((
        SELECT SUM(COALESCE((e.payload->>'input_tokens_uncached')::INT, 0))
        FROM events e
        WHERE e.session_id = s.id AND e.event_type = 'BrainResponse'
    ), 0),
    -- ... same for the other three counters and cost
;
```

Wrap this in a one-shot migration step. Run once per deployment. Log counts affected.

### 5j. Documentation

`moa/docs/analytics.md` (new): describes the three views, their refresh semantics, and how to add new analytic queries. Warn readers: "Generated columns and triggers are the source of truth for aggregates. Do not write aggregates from application code."

---

## 6. Deliverables

- [ ] Migration adding `total_input_tokens` and `cache_hit_rate` as generated columns on `sessions`.
- [ ] Migration adding `update_session_aggregates` trigger.
- [ ] Migration defining `tool_call_analytics`, `tool_call_summary`, `session_summary` views.
- [ ] Migration defining `daily_workspace_metrics` materialized view + periodic refresh task.
- [ ] Backfill SQL for existing data.
- [ ] Application code deleted: `UPDATE sessions SET total_...`, in-memory summing, per-tool aggregation helpers.
- [ ] Rust helpers in `moa-core/src/analytics.rs` for typed reads from views.
- [ ] CLI subcommands: `moa session stats`, `moa workspace stats`, `moa tool stats`, `moa cache stats`.
- [ ] Tests verifying trigger correctness, view correctness, rollback on trigger failure.
- [ ] `moa/docs/analytics.md` explaining the model.
- [ ] Grep confirms no application-side aggregation remains.

---

## 7. Acceptance criteria

1. Inserting a `BrainResponse` event updates the session's counters automatically — no application call needed.
2. `session_summary.cache_hit_rate` agrees with step 79's log output to 4 decimal places (they're now computing the same thing, just from different sides of the same equation).
3. `tool_call_summary` reports reasonable p50/p95 latencies that match spans in Jaeger from step 81.
4. `grep -rn 'UPDATE sessions SET total' moa-session/` returns zero matches after the cleanup.
5. Application binary size drops (less code). Measured as a signal, not a target.
6. Deleting `total_input_tokens` as a stored column manually (dev-only), then re-reading a session, shows the view still returns correct numbers via the generated column (verifies the generation rule works even after a DBA-level accident).
7. `cargo test --workspace` green.
8. `REFRESH MATERIALIZED VIEW CONCURRENTLY daily_workspace_metrics` runs in under 5 seconds at 100K sessions.
9. Step 78 integration test still passes. Step 79's cache hit rate assertion now reads from `cache_hit_rate` on the session row.
