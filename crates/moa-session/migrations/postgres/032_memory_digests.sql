CREATE TABLE IF NOT EXISTS moa.memory_digests (
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    content TEXT NOT NULL,
    source_fact_uids JSONB NOT NULL DEFAULT '[]'::jsonb,
    version INTEGER NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    CHECK (scope IN ('user', 'workspace')),
    CHECK (scope IS NOT NULL)
);

CREATE UNIQUE INDEX IF NOT EXISTS memory_digests_identity
    ON moa.memory_digests (workspace_id, scope, COALESCE(user_id, ''));

CREATE INDEX IF NOT EXISTS memory_digests_updated_at_idx
    ON moa.memory_digests (workspace_id, updated_at);

GRANT USAGE ON SCHEMA moa TO moa_app, moa_promoter;

SELECT moa.apply_three_tier_rls('moa.memory_digests'::REGCLASS);
