CREATE EXTENSION IF NOT EXISTS vector;

DROP TABLE IF EXISTS moa.embeddings_old CASCADE;

CREATE TABLE IF NOT EXISTS moa.embeddings (
    uid UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    label TEXT NOT NULL CHECK (label = ANY(moa.age_vertex_labels())),
    pii_class TEXT NOT NULL DEFAULT 'none'
        CHECK (pii_class IN ('none', 'pii', 'phi', 'restricted')),
    embedding halfvec(1024) NOT NULL,
    embedding_model TEXT NOT NULL,
    embedding_model_version INT NOT NULL,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
) PARTITION BY HASH (workspace_id);

DO $$
DECLARE
    partition_index INT;
BEGIN
    FOR partition_index IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS moa.embeddings_p%s
             PARTITION OF moa.embeddings
             FOR VALUES WITH (MODULUS 32, REMAINDER %s)',
            lpad(partition_index::TEXT, 2, '0'),
            partition_index
        );
    END LOOP;
END $$;

CREATE UNIQUE INDEX IF NOT EXISTS embeddings_workspace_uid_unique
    ON moa.embeddings (workspace_id, uid) NULLS NOT DISTINCT;
CREATE INDEX IF NOT EXISTS embeddings_embedding_hnsw_idx
    ON moa.embeddings USING hnsw (embedding halfvec_cosine_ops)
    WITH (m = 16, ef_construction = 64);
CREATE INDEX IF NOT EXISTS embeddings_ws_scope_label_idx
    ON moa.embeddings (workspace_id, scope, label)
    WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS embeddings_uid_idx
    ON moa.embeddings (uid);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;
SELECT moa.apply_three_tier_rls('moa.embeddings'::REGCLASS);
