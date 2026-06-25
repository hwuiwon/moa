
-- Source: 024_lineage.sql

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_available_extensions WHERE name = 'timescaledb') THEN
        BEGIN
            CREATE EXTENSION IF NOT EXISTS timescaledb;
        EXCEPTION WHEN OTHERS THEN
            RAISE NOTICE 'TimescaleDB extension is available but could not be created: %', SQLERRM;
        END;
    END IF;
END
$$;

CREATE SCHEMA IF NOT EXISTS analytics;

CREATE TABLE IF NOT EXISTS analytics.turn_lineage (
    turn_id        UUID        NOT NULL,
    session_id     UUID        NOT NULL,
    user_id        TEXT        NOT NULL,
    storage_partition_id   TEXT        NOT NULL,
    ts             TIMESTAMPTZ NOT NULL,
    tier           SMALLINT    NOT NULL DEFAULT 1,
    record_kind    SMALLINT    NOT NULL,
    payload        JSONB       NOT NULL,
    answer_text    TEXT,
    integrity_hash BYTEA       NOT NULL,
    prev_hash      BYTEA,
    PRIMARY KEY (turn_id, record_kind, ts)
);

CREATE INDEX IF NOT EXISTS ix_lineage_session_ts
    ON analytics.turn_lineage (session_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_storage_partition_user_ts
    ON analytics.turn_lineage (storage_partition_id, user_id, ts DESC);

CREATE INDEX IF NOT EXISTS ix_lineage_zero_recall
    ON analytics.turn_lineage (ts DESC)
    WHERE record_kind = 1
      AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
      AND jsonb_array_length(payload #> '{record,top_k}') = 0;

CREATE INDEX IF NOT EXISTS ix_lineage_payload_gin
    ON analytics.turn_lineage
    USING GIN ((payload) jsonb_path_ops);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb') THEN
        PERFORM create_hypertable(
            'analytics.turn_lineage',
            'ts',
            chunk_time_interval => INTERVAL '1 day',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            ALTER TABLE analytics.turn_lineage SET (
                timescaledb.compress,
                timescaledb.compress_segmentby = 'storage_partition_id',
                timescaledb.compress_orderby = 'ts DESC, turn_id'
            )
        $ddl$;

        PERFORM add_compression_policy(
            'analytics.turn_lineage',
            INTERVAL '7 days',
            if_not_exists => TRUE
        );
        PERFORM add_retention_policy(
            'analytics.turn_lineage',
            INTERVAL '30 days',
            if_not_exists => TRUE
        );

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly
            WITH (timescaledb.continuous) AS
            SELECT time_bucket('1 hour', ts) AS bucket,
                   storage_partition_id,
                   COUNT(*) AS turns,
                   COUNT(*) FILTER (
                       WHERE record_kind = 1
                         AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
                         AND jsonb_array_length(payload #> '{record,top_k}') = 0
                   ) AS zero_recall
            FROM analytics.turn_lineage
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        PERFORM add_continuous_aggregate_policy(
            'analytics.turn_recall_hourly',
            start_offset => INTERVAL '7 days',
            end_offset => INTERVAL '5 minutes',
            schedule_interval => INTERVAL '5 minutes',
            if_not_exists => TRUE
        );
    ELSE
        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.turn_recall_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   storage_partition_id,
                   COUNT(*) AS turns,
                   COUNT(*) FILTER (
                       WHERE record_kind = 1
                         AND jsonb_typeof(payload #> '{record,top_k}') = 'array'
                         AND jsonb_array_length(payload #> '{record,top_k}') = 0
                   ) AS zero_recall
            FROM analytics.turn_lineage
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;
    END IF;
END
$$;

-- Source: 025_lineage_scores.sql

CREATE TABLE IF NOT EXISTS analytics.scores (
    score_id           UUID             NOT NULL,
    ts                 TIMESTAMPTZ      NOT NULL,
    storage_partition_id       TEXT             NOT NULL,
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

CREATE INDEX IF NOT EXISTS ix_scores_storage_partition_name_ts
    ON analytics.scores (storage_partition_id, name, ts DESC);

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
    storage_partition_id    TEXT        NOT NULL,
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
                timescaledb.compress_segmentby = 'storage_partition_id, name',
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
                   storage_partition_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, storage_partition_id
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
                   storage_partition_id,
                   AVG(value_numeric) AS p50,
                   MAX(value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, storage_partition_id
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
                   storage_partition_id,
                   AVG(CASE WHEN value_boolean THEN 1.0 ELSE 0.0 END) AS verified_rate,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'citation_verified' AND value_type = 'boolean'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;

        EXECUTE $ddl$
            CREATE MATERIALIZED VIEW IF NOT EXISTS analytics.nli_hourly AS
            SELECT date_trunc('hour', ts) AS bucket,
                   storage_partition_id,
                   percentile_cont(0.5) WITHIN GROUP (ORDER BY value_numeric) AS p50,
                   percentile_cont(0.95) WITHIN GROUP (ORDER BY value_numeric) AS p95,
                   AVG(value_numeric) AS mean,
                   COUNT(*) AS n
            FROM analytics.scores
            WHERE name = 'nli_entailment' AND value_type = 'numeric'
            GROUP BY bucket, storage_partition_id
            WITH NO DATA
        $ddl$;
    END IF;
END
$$;

-- Source: 026_lineage_audit.sql

CREATE TABLE IF NOT EXISTS analytics.compliance_tenants (
    storage_partition_id       TEXT PRIMARY KEY,
    enabled            BOOLEAN     NOT NULL DEFAULT TRUE,
    enabled_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    retention_years    INT         NOT NULL DEFAULT 10,
    s3_bucket          TEXT        NOT NULL,
    kms_key_id         TEXT,
    signing_key_label  TEXT        NOT NULL,
    notes              TEXT
);

CREATE TABLE IF NOT EXISTS analytics.compliance_storage_partition_state (
    storage_partition_id          TEXT PRIMARY KEY,
    last_integrity_hash   BYTEA,
    last_ts               TIMESTAMPTZ,
    record_count          BIGINT NOT NULL DEFAULT 0,
    last_root_id          UUID
);

CREATE TABLE IF NOT EXISTS analytics.audit_roots (
    root_id            UUID PRIMARY KEY,
    storage_partition_id       TEXT        NOT NULL,
    window_start       TIMESTAMPTZ NOT NULL,
    window_end         TIMESTAMPTZ NOT NULL,
    record_count       BIGINT      NOT NULL,
    merkle_root        BYTEA       NOT NULL,
    signature          BYTEA       NOT NULL,
    signing_key_label  TEXT        NOT NULL,
    s3_object_uri      TEXT        NOT NULL,
    s3_object_etag     TEXT        NOT NULL,
    object_lock_mode   TEXT        NOT NULL,
    retain_until       TIMESTAMPTZ NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS ix_audit_roots_storage_partition_window
    ON analytics.audit_roots (storage_partition_id, window_end DESC);

CREATE SCHEMA IF NOT EXISTS pii_vault;

CREATE TABLE IF NOT EXISTS pii_vault.subject_keys (
    subject_pseudonym BYTEA PRIMARY KEY,
    storage_partition_id      TEXT        NOT NULL,
    hmac_key_handle   TEXT        NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    erased_at         TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS pii_vault.plaintext_side (
    record_id          UUID PRIMARY KEY,
    subject_pseudonym  BYTEA       NOT NULL,
    storage_partition_id       TEXT        NOT NULL,
    field_name         TEXT        NOT NULL,
    ciphertext         BYTEA       NOT NULL,
    encryption_context JSONB       NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    FOREIGN KEY (subject_pseudonym) REFERENCES pii_vault.subject_keys(subject_pseudonym)
);

CREATE INDEX IF NOT EXISTS ix_plaintext_subject
    ON pii_vault.plaintext_side (subject_pseudonym);

CREATE INDEX IF NOT EXISTS ix_plaintext_storage_partition
    ON pii_vault.plaintext_side (storage_partition_id, created_at);

-- Source: 028_lineage_dead_letters.sql

-- Dead-letter storage for lineage writer batches that cannot be written after bounded retries.

CREATE TABLE IF NOT EXISTS analytics.lineage_dead_letters (
    dead_letter_id      UUID        PRIMARY KEY,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    error               TEXT        NOT NULL,
    attempts            INTEGER     NOT NULL,
    row_count           INTEGER     NOT NULL,
    first_storage_partition_id  TEXT,
    first_session_id    UUID,
    first_turn_id       UUID,
    rows                JSONB       NOT NULL
);

CREATE INDEX IF NOT EXISTS lineage_dead_letters_created_idx
    ON analytics.lineage_dead_letters (created_at DESC);
