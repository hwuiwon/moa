CREATE EXTENSION IF NOT EXISTS vector;

ALTER TABLE wiki_pages
    ADD COLUMN IF NOT EXISTS embedding vector(1536),
    ADD COLUMN IF NOT EXISTS embedding_model TEXT,
    ADD COLUMN IF NOT EXISTS embedding_updated TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS wiki_pages_embedding_hnsw
    ON wiki_pages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE TABLE IF NOT EXISTS wiki_embedding_queue (
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    path TEXT NOT NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    CHECK (scope IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS wiki_embedding_queue_scope_path_unique
    ON wiki_embedding_queue (workspace_id, user_id, path) NULLS NOT DISTINCT;
CREATE INDEX IF NOT EXISTS wiki_embedding_queue_enqueued
    ON wiki_embedding_queue (enqueued_at);

SELECT moa.apply_three_tier_rls('wiki_pages'::REGCLASS);
SELECT moa.apply_three_tier_rls('wiki_embedding_queue'::REGCLASS);
