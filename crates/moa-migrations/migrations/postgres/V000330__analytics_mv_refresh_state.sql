-- Freshness and outcome state for the analytics materialized-view refresh.
--
-- Refresh ownership moved off the edge request path (where every replica could
-- herd a refresh) to the durable maintenance cron, single-flighted under a
-- Postgres advisory lock. This singleton row records the last successful and
-- failed refresh so the edge can report read-model staleness without triggering
-- work, and operators can see refresh health.
--
-- Deployment-global (one row, keyed by a constant), so no tenant RLS. It follows
-- the `analytics.clickhouse_export_state` precedent: owned by the migration
-- role, no explicit per-role grants.

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.materialized_view_refresh_state (
    -- Singleton guard: only the `true` row exists.
    id BOOLEAN PRIMARY KEY DEFAULT TRUE,
    -- Completion time of the most recent fully successful refresh.
    last_success_at TIMESTAMPTZ,
    -- Start/observation time of the most recent failed refresh.
    last_failure_at TIMESTAMPTZ,
    -- Error text of the most recent failed refresh.
    last_error TEXT,
    -- Wall-clock duration of the most recent refresh attempt, in milliseconds.
    last_duration_ms BIGINT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT materialized_view_refresh_state_singleton CHECK (id)
);
