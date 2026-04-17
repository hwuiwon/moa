CREATE EXTENSION IF NOT EXISTS vector;

ALTER TABLE wiki_pages
    ADD COLUMN IF NOT EXISTS embedding vector(1536),
    ADD COLUMN IF NOT EXISTS embedding_model TEXT,
    ADD COLUMN IF NOT EXISTS embedding_updated TIMESTAMPTZ;

CREATE INDEX IF NOT EXISTS wiki_pages_embedding_hnsw
    ON wiki_pages USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

CREATE TABLE IF NOT EXISTS wiki_embedding_queue (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (scope, path)
);

CREATE INDEX IF NOT EXISTS wiki_embedding_queue_enqueued
    ON wiki_embedding_queue (enqueued_at);
