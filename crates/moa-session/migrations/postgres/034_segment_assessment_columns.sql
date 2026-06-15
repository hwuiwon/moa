DROP MATERIALIZED VIEW IF EXISTS skill_resolution_rates;
DROP MATERIALIZED VIEW IF EXISTS segment_baselines;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'outcome'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution TO outcome;
    END IF;

    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution_signal'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'assessment'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution_signal TO assessment;
    END IF;

    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'resolution_confidence'
    )
    AND NOT EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'task_segments'
          AND column_name = 'outcome_confidence'
    ) THEN
        ALTER TABLE task_segments RENAME COLUMN resolution_confidence TO outcome_confidence;
    END IF;
END $$;

DROP INDEX IF EXISTS idx_task_segments_tenant_resolution;
CREATE INDEX IF NOT EXISTS idx_task_segments_tenant_outcome
    ON task_segments (tenant_id, outcome);

CREATE MATERIALIZED VIEW skill_resolution_rates AS
SELECT
    t.tenant_id,
    unnest(t.skills_activated) AS skill_name,
    COUNT(*)::BIGINT AS uses,
    AVG(CASE WHEN t.outcome = 'resolved' THEN 1.0
             WHEN t.outcome = 'partial' THEN 0.5
             ELSE 0.0 END)::DOUBLE PRECISION AS resolution_rate,
    AVG(t.token_cost)::DOUBLE PRECISION AS avg_token_cost,
    AVG(t.turn_count)::DOUBLE PRECISION AS avg_turn_count
FROM task_segments t
WHERE t.outcome IS NOT NULL
  AND array_length(t.skills_activated, 1) IS NOT NULL
GROUP BY t.tenant_id, skill_name;

CREATE UNIQUE INDEX IF NOT EXISTS idx_skill_resolution_rates_unique
    ON skill_resolution_rates(tenant_id, skill_name);

CREATE MATERIALIZED VIEW segment_baselines AS
SELECT
    tenant_id,
    COUNT(*)::BIGINT AS sample_count,
    AVG(turn_count)::DOUBLE PRECISION AS avg_turns,
    STDDEV(turn_count)::DOUBLE PRECISION AS stddev_turns,
    AVG(token_cost)::DOUBLE PRECISION AS avg_cost,
    STDDEV(token_cost)::DOUBLE PRECISION AS stddev_cost,
    AVG(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS avg_duration_secs,
    STDDEV(EXTRACT(EPOCH FROM (ended_at - started_at)))::DOUBLE PRECISION AS stddev_duration_secs
FROM task_segments
WHERE ended_at IS NOT NULL
GROUP BY tenant_id;

CREATE UNIQUE INDEX IF NOT EXISTS idx_segment_baselines_unique
    ON segment_baselines(tenant_id);
