-- ClickHouse analytics exporter: Postgres-side cursor state plus `updated_at`
-- coverage for the mutable source tables the exporter pulls incrementally.
--
-- The exporter (`crates/moa-orchestrator/src/analytics_export`) pulls changed
-- rows by an `updated_at` cursor with a `2 × export_poll_secs` overlap, so every
-- exported mutable table needs an `updated_at` that advances on every UPDATE and
-- an index that keeps the cursor pull an index-range read.
--
-- Audit of the exported source tables (verified against the store update paths):
--   * sessions, moa.artifact_run, moa.artifact_node_run, moa.experiment_run,
--     learning_candidates — every UPDATE path sets `updated_at`; no change here
--     beyond a cursor index.
--   * session_agent_context — insert-only (no UPDATE path); `updated_at`
--     equals `created_at` for the life of the row, which the cursor handles.
--   * task_segments — had NO `updated_at` column at all, and its store mutates
--     rows in place (complete / assessment / turn+cost increment / tool append),
--     so this migration adds the column plus a BEFORE UPDATE trigger.

CREATE SCHEMA IF NOT EXISTS analytics;

-- Per-table export cursor. `cursor_ts` is the exported-through `updated_at`
-- (dims) or event `timestamp` (events_raw); `cursor_id` breaks ties for the
-- append-only events stream. ReplacingMergeTree on the ClickHouse side absorbs
-- the rows re-read by the overlap window.
CREATE TABLE IF NOT EXISTS analytics.clickhouse_export_state (
    table_name  TEXT PRIMARY KEY,
    cursor_ts   TIMESTAMPTZ NOT NULL,
    cursor_id   UUID,
    exported_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Generic "bump updated_at on every UPDATE" trigger for tables whose store code
-- does not set the column itself.
CREATE OR REPLACE FUNCTION moa.touch_updated_at() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.updated_at := NOW();
    RETURN NEW;
END;
$$;

ALTER TABLE task_segments
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW();
DROP TRIGGER IF EXISTS task_segments_touch_updated_at ON task_segments;
CREATE TRIGGER task_segments_touch_updated_at
    BEFORE UPDATE ON task_segments
    FOR EACH ROW
    EXECUTE FUNCTION moa.touch_updated_at();

-- Cursor indexes so each incremental `updated_at` range read is an index scan
-- rather than a sequential scan of the source table. The existing session
-- indexes all lead with another column (tenant_id / storage_partition_id /
-- user_id), so none can serve the unqualified `WHERE updated_at > $cursor` pull.
CREATE INDEX IF NOT EXISTS idx_sessions_updated
    ON sessions(updated_at);
CREATE INDEX IF NOT EXISTS idx_task_segments_updated
    ON task_segments(updated_at);
CREATE INDEX IF NOT EXISTS idx_session_agent_context_updated
    ON session_agent_context(updated_at);
CREATE INDEX IF NOT EXISTS idx_learning_candidates_updated
    ON learning_candidates(updated_at);
CREATE INDEX IF NOT EXISTS idx_artifact_run_updated
    ON moa.artifact_run(updated_at);
CREATE INDEX IF NOT EXISTS idx_artifact_node_run_updated
    ON moa.artifact_node_run(updated_at);
CREATE INDEX IF NOT EXISTS idx_experiment_run_updated
    ON moa.experiment_run(updated_at);
