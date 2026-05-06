CREATE TABLE IF NOT EXISTS analytics.scores (
    score_id           UUID             NOT NULL,
    ts                 TIMESTAMPTZ      NOT NULL,
    workspace_id       TEXT             NOT NULL,
    user_id            TEXT,
    target_kind        TEXT             NOT NULL,
    turn_id            UUID,
    session_id         UUID,
    run_id             UUID,
    item_id            UUID,
    dataset_id         UUID,
    name               TEXT             NOT NULL,
    value_type         TEXT             NOT NULL,
    value_numeric      DOUBLE PRECISION,
    value_boolean      BOOLEAN,
    value_categorical  TEXT,
    source             TEXT             NOT NULL,
    model_or_evaluator TEXT             NOT NULL,
    comment            TEXT,
    PRIMARY KEY (score_id, ts)
);

CREATE INDEX IF NOT EXISTS ix_scores_workspace_name_ts
    ON analytics.scores (workspace_id, name, ts DESC);

CREATE INDEX IF NOT EXISTS ix_scores_turn
    ON analytics.scores (turn_id)
    WHERE turn_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS ix_scores_run
    ON analytics.scores (run_id)
    WHERE run_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS analytics.eval_datasets (
    dataset_id  UUID        PRIMARY KEY,
    name        TEXT        NOT NULL UNIQUE,
    source_path TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS analytics.eval_dataset_items (
    item_id         UUID        PRIMARY KEY,
    dataset_id      UUID        NOT NULL REFERENCES analytics.eval_datasets(dataset_id) ON DELETE CASCADE,
    workspace_id    TEXT        NOT NULL,
    scope           JSONB       NOT NULL,
    query           TEXT        NOT NULL,
    expected_answer TEXT,
    expected_chunk_ids UUID[]   NOT NULL DEFAULT '{}',
    metadata        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_eval_dataset_items_dataset
    ON analytics.eval_dataset_items (dataset_id, created_at ASC);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb') THEN
        PERFORM create_hypertable(
            'analytics.scores',
            'ts',
            chunk_time_interval => INTERVAL '1 day',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            ALTER TABLE analytics.scores SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'workspace_id, name',
                timescaledb.compress_orderby = 'ts DESC'
            )
        $ddl$;

        PERFORM add_compression_policy(
            'analytics.scores',
            INTERVAL '7 days',
            if_not_exists => TRUE
        );
        PERFORM add_retention_policy(
            'analytics.scores',
            INTERVAL '90 days',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.grounding_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   workspace_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, workspace_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.grounding_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.nli_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   workspace_id,
                   AVG(value_numeric) AS p50,
                   MAX(value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, workspace_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.nli_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );
    ELSE
        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.grounding_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   workspace_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, workspace_id
            WITH NO DATA
        $ddl$;

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.nli_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   workspace_id,
                   percentile_cont(0.5) WITHIN GROUP (ORDER BY value_numeric) AS p50,
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, workspace_id
            WITH NO DATA
        $ddl$;
    END IF;
END
$$;
