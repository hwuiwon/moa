CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE IF NOT EXISTS wiki_pages (
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    page_type TEXT NOT NULL,
    confidence TEXT NOT NULL,
    created TIMESTAMPTZ NOT NULL,
    updated TIMESTAMPTZ NOT NULL,
    last_referenced TIMESTAMPTZ NOT NULL,
    reference_count INTEGER NOT NULL DEFAULT 0,
    tags TEXT[] NOT NULL DEFAULT '{}',
    content TEXT NOT NULL,
    search_tsv TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
        setweight(array_to_tsvector(coalesce(tags, ARRAY[]::text[])), 'B') ||
        setweight(to_tsvector('english', coalesce(content, '')), 'C')
    ) STORED,
    CHECK (scope IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS wiki_pages_scope_path_unique
    ON wiki_pages (workspace_id, user_id, path) NULLS NOT DISTINCT;
CREATE INDEX IF NOT EXISTS wiki_pages_tsv_gin ON wiki_pages USING GIN (search_tsv);
CREATE INDEX IF NOT EXISTS wiki_pages_title_trgm ON wiki_pages USING GIN (title gin_trgm_ops);
CREATE INDEX IF NOT EXISTS wiki_pages_tags_gin ON wiki_pages USING GIN (tags);
CREATE INDEX IF NOT EXISTS wiki_pages_updated
    ON wiki_pages (workspace_id, scope, user_id, updated DESC);
CREATE INDEX IF NOT EXISTS wiki_pages_type
    ON wiki_pages (workspace_id, scope, user_id, page_type);
