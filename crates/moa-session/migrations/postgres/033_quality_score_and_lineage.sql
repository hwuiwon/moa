ALTER TABLE moa.node_index
    ADD COLUMN IF NOT EXISTS quality_score DOUBLE PRECISION NOT NULL DEFAULT 0.5;

CREATE TABLE IF NOT EXISTS moa.retrieval_lineage (
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    session_id UUID NOT NULL,
    turn_seq BIGINT NOT NULL,
    uid UUID NOT NULL,
    rank INTEGER NOT NULL CHECK (rank > 0),
    retrieved_at TIMESTAMPTZ NOT NULL,
    CHECK (scope = 'user')
);

CREATE INDEX IF NOT EXISTS retrieval_lineage_ws_time
    ON moa.retrieval_lineage (workspace_id, retrieved_at);
CREATE INDEX IF NOT EXISTS retrieval_lineage_uid_time
    ON moa.retrieval_lineage (uid, retrieved_at);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.retrieval_lineage'::REGCLASS);
