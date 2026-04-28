CREATE TABLE IF NOT EXISTS moa.skill_addendum (
    addendum_uid UUID PRIMARY KEY,
    skill_uid UUID NOT NULL REFERENCES moa.skill(skill_uid) ON DELETE CASCADE,
    linked_lesson_uid UUID NOT NULL REFERENCES moa.node_index(uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    summary TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    valid_to TIMESTAMPTZ,
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS skill_addendum_skill_idx
    ON moa.skill_addendum (skill_uid)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS skill_addendum_lesson_idx
    ON moa.skill_addendum (linked_lesson_uid);

CREATE INDEX IF NOT EXISTS skill_addendum_scope_idx
    ON moa.skill_addendum (workspace_id, scope, user_id)
    WHERE valid_to IS NULL;

SELECT moa.apply_three_tier_rls('moa.skill_addendum'::REGCLASS);
