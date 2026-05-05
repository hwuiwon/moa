CREATE TABLE IF NOT EXISTS moa.skill (
    skill_uid UUID PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    name TEXT NOT NULL,
    description TEXT,
    body TEXT NOT NULL,
    body_hash BYTEA NOT NULL,
    version INT NOT NULL DEFAULT 1,
    previous_skill_uid UUID REFERENCES moa.skill(skill_uid),
    tags TEXT[],
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (version > 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS skill_active_name_uniq
    ON moa.skill (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        name
    )
    WHERE valid_to IS NULL;

CREATE UNIQUE INDEX IF NOT EXISTS skill_active_name_body_hash_uniq
    ON moa.skill (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        name,
        body_hash
    )
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS skill_tags_gin
    ON moa.skill USING GIN (tags);

CREATE INDEX IF NOT EXISTS skill_scope_idx
    ON moa.skill (workspace_id, scope, user_id)
    WHERE valid_to IS NULL;

SELECT moa.apply_three_tier_rls('moa.skill'::REGCLASS);
