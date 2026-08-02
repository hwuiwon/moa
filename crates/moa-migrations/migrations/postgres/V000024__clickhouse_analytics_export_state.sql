-- ClickHouse analytics exporter: Postgres-side cursor state plus `updated_at`
-- coverage for the mutable source tables the exporter pulls incrementally.
--
-- The exporter (`crates/moa-analytics-export`) pulls changed
-- rows by an `updated_at` cursor with a `2 × export_poll_secs` overlap, so every
-- exported mutable table needs an `updated_at` that advances on every UPDATE and
-- an index matching the `(updated_at, primary_key)` pagination order.
--
-- Audit of the exported source tables (verified against the store update paths):
--   * sessions, moa.experiment_run, learning_candidates — every UPDATE path
--     sets `updated_at`; no change here beyond a cursor index.
--   * session_agent_context — insert-only (no UPDATE path); `updated_at`
--     equals `created_at` for the life of the row, which the cursor handles.
--   * task_segments — its store mutates rows in place (complete / assessment /
--     turn+cost increment / tool append), so the trigger advances `updated_at`.

-- Per-table export cursor. `cursor_ts` is the exported-through `updated_at`
-- (dims) or event `timestamp` (events_raw); `cursor_id` breaks ties for the
-- append-only events stream. ReplacingMergeTree on the ClickHouse side absorbs
-- the rows re-read by the overlap window.
CREATE TABLE analytics.clickhouse_export_state (
    table_name         TEXT PRIMARY KEY,
    cursor_ts          TIMESTAMPTZ NOT NULL,
    cursor_id          UUID,
    exported_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    cursor_seq         BIGINT,
    pass_high_water_seq BIGINT,
    pass_high_water_id UUID,
    pass_started_at    TIMESTAMPTZ,
    CONSTRAINT clickhouse_export_state_cursor_sequence_check CHECK (
        cursor_seq IS NULL
        OR (cursor_seq >= 0 AND cursor_id IS NOT NULL)
    ),
    CONSTRAINT clickhouse_export_state_pass_high_water_check CHECK (
        (
            pass_high_water_seq IS NULL
            AND pass_high_water_id IS NULL
            AND pass_started_at IS NULL
        )
        OR (
            cursor_seq IS NOT NULL
            AND cursor_id IS NOT NULL
            AND pass_high_water_seq IS NOT NULL
            AND pass_high_water_seq >= 0
            AND pass_high_water_id IS NOT NULL
            AND pass_started_at IS NOT NULL
            AND ROW(pass_high_water_seq, pass_high_water_id)
                >= ROW(cursor_seq, cursor_id)
        )
    )
);

-- Keep the mutable task-segment export cursor current even when a store update
-- does not explicitly touch the timestamp.
CREATE FUNCTION moa.set_task_segment_updated_at() RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = pg_catalog
AS $$
BEGIN
    NEW.updated_at := NOW();
    RETURN NEW;
END;
$$;

CREATE TRIGGER task_segments_touch_updated_at
    BEFORE UPDATE ON task_segments
    FOR EACH ROW
    EXECUTE FUNCTION moa.set_task_segment_updated_at();

-- Cursor indexes so each incremental `updated_at` range read is an index scan
-- rather than a sequential scan of the source table. The existing session
-- indexes all lead with another column (tenant_id / storage_partition_id /
-- user_id), so none can serve the unqualified `WHERE updated_at > $cursor` pull.
CREATE INDEX idx_sessions_updated
    ON sessions(updated_at, id);
CREATE INDEX idx_task_segments_updated
    ON task_segments(updated_at, id);
CREATE INDEX idx_session_agent_context_updated
    ON session_agent_context(updated_at, session_id);
CREATE INDEX idx_learning_candidates_updated
    ON learning_candidates(updated_at, id);
CREATE INDEX idx_experiment_run_updated
    ON moa.experiment_run(updated_at, run_uid);
