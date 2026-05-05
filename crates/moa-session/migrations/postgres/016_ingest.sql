CREATE TABLE IF NOT EXISTS moa.ingest_dedup (
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    session_id UUID NOT NULL,
    turn_seq BIGINT NOT NULL,
    fact_hash BYTEA NOT NULL,
    fact_uid UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, session_id, turn_seq, fact_hash),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS ingest_dedup_fact_uid_idx
    ON moa.ingest_dedup (fact_uid);
CREATE INDEX IF NOT EXISTS ingest_dedup_session_idx
    ON moa.ingest_dedup (workspace_id, session_id, turn_seq);

CREATE TABLE IF NOT EXISTS moa.ingest_dlq (
    dlq_id BIGSERIAL PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    session_id UUID,
    turn_seq BIGINT,
    payload JSONB NOT NULL,
    error TEXT NOT NULL,
    retry_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    next_retry_at TIMESTAMPTZ,
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS ingest_dlq_retry_idx
    ON moa.ingest_dlq (workspace_id, next_retry_at, retry_count);
CREATE INDEX IF NOT EXISTS ingest_dlq_session_idx
    ON moa.ingest_dlq (workspace_id, session_id, turn_seq);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
GRANT USAGE, SELECT ON SEQUENCE moa.ingest_dlq_dlq_id_seq TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.ingest_dedup'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.ingest_dlq'::REGCLASS);

ALTER TABLE moa.workspace_state
    ADD COLUMN IF NOT EXISTS slow_path_degraded BOOLEAN NOT NULL DEFAULT false;

ALTER TABLE moa.workspace_state
    ADD COLUMN IF NOT EXISTS ingest_concurrency INT NOT NULL DEFAULT 8;

ALTER TABLE moa.workspace_state
    DROP CONSTRAINT IF EXISTS workspace_state_ingest_concurrency_positive;

ALTER TABLE moa.workspace_state
    ADD CONSTRAINT workspace_state_ingest_concurrency_positive
    CHECK (ingest_concurrency > 0);
