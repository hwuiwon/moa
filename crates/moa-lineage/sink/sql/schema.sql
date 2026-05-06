CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.turn_lineage (
    turn_id        UUID        NOT NULL,
    session_id     UUID        NOT NULL,
    user_id        TEXT        NOT NULL,
    workspace_id   TEXT        NOT NULL,
    ts             TIMESTAMPTZ NOT NULL,
    tier           SMALLINT    NOT NULL DEFAULT 1,
    record_kind    SMALLINT    NOT NULL,
    payload        JSONB       NOT NULL,
    answer_text    TEXT,
    integrity_hash BYTEA       NOT NULL,
    prev_hash      BYTEA,
    PRIMARY KEY (turn_id, record_kind, ts)
);

SELECT create_hypertable(
    'analytics.turn_lineage',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists => TRUE
);

ALTER TABLE analytics.turn_lineage SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'workspace_id',
    timescaledb.compress_orderby = 'ts DESC, turn_id'
);

SELECT add_compression_policy('analytics.turn_lineage', INTERVAL '7 days', if_not_exists => TRUE);
SELECT add_retention_policy('analytics.turn_lineage', INTERVAL '30 days', if_not_exists => TRUE);

CREATE INDEX IF NOT EXISTS ix_lineage_session_ts
    ON analytics.turn_lineage (session_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_workspace_user_ts
    ON analytics.turn_lineage (workspace_id, user_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_zero_recall
    ON analytics.turn_lineage (ts DESC)
    WHERE record_kind = 1
      AND jsonb_array_length(payload #> '{record,top_k}') = 0;

CREATE INDEX IF NOT EXISTS ix_lineage_payload_gin
    ON analytics.turn_lineage
    USING GIN ((payload) jsonb_path_ops);

CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly
WITH (timescaledb.continuous) AS
SELECT time_bucket('1 hour', ts) AS bucket,
       workspace_id,
       COUNT(*) AS turns,
       COUNT(*) FILTER (
           WHERE record_kind = 1
             AND jsonb_array_length(payload #> '{record,top_k}') = 0
       ) AS zero_recall
FROM analytics.turn_lineage
GROUP BY bucket, workspace_id
WITH NO DATA;

SELECT add_continuous_aggregate_policy(
    'analytics.turn_recall_hourly',
    start_offset => INTERVAL '7 days',
    end_offset => INTERVAL '5 minutes',
    schedule_interval => INTERVAL '5 minutes',
    if_not_exists => TRUE
);
