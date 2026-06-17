CREATE TABLE IF NOT EXISTS analytics.score_run (
    run_id UUID PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    source TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS score_run_scope_source_idx
    ON analytics.score_run (workspace_id, scope, user_id, source, created_at DESC);

SELECT moa.apply_three_tier_rls('analytics.score_run'::REGCLASS);

CREATE TABLE IF NOT EXISTS moa.experiment_run (
    run_uid UUID PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    name TEXT NOT NULL,
    target_kind TEXT NOT NULL CHECK (target_kind IN ('agent_loop', 'workflow')),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'running', 'waiting_approval', 'completed', 'failed', 'cancelled')),
    target JSONB NOT NULL,
    variant JSONB NOT NULL,
    scorecard JSONB NOT NULL DEFAULT '{}'::jsonb,
    score_run_id UUID NOT NULL REFERENCES analytics.score_run(run_id) ON DELETE RESTRICT,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    workflow_run_uid UUID REFERENCES moa.artifact_run(run_uid) ON DELETE SET NULL,
    artifact_revision_uids UUID[] NOT NULL DEFAULT '{}',
    idempotency_key TEXT,
    created_by_identity JSONB NOT NULL,
    error TEXT,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

INSERT INTO analytics.score_run (run_id, workspace_id, user_id, source)
SELECT DISTINCT score_run_id, workspace_id, user_id, 'experiment_run'
FROM moa.experiment_run
WHERE score_run_id IS NOT NULL
ON CONFLICT (run_id) DO NOTHING;

ALTER TABLE moa.experiment_run
    DROP CONSTRAINT IF EXISTS experiment_run_score_run_id_fkey,
    ADD CONSTRAINT experiment_run_score_run_id_fkey
        FOREIGN KEY (score_run_id)
        REFERENCES analytics.score_run(run_id)
        ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS experiment_run_scope_idx
    ON moa.experiment_run (workspace_id, scope, user_id, status, started_at DESC);

CREATE INDEX IF NOT EXISTS experiment_run_score_run_idx
    ON moa.experiment_run (score_run_id);

CREATE INDEX IF NOT EXISTS experiment_run_session_idx
    ON moa.experiment_run (session_id)
    WHERE session_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS experiment_run_workflow_run_idx
    ON moa.experiment_run (workflow_run_uid)
    WHERE workflow_run_uid IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS experiment_run_idempotency_uniq
    ON moa.experiment_run (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        idempotency_key
    )
    WHERE idempotency_key IS NOT NULL;

SELECT moa.apply_three_tier_rls('moa.experiment_run'::REGCLASS);

CREATE TABLE IF NOT EXISTS moa.experiment_run_artifact_revision (
    run_uid UUID NOT NULL REFERENCES moa.experiment_run(run_uid) ON DELETE CASCADE,
    revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_uid, revision_uid),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS experiment_run_artifact_revision_revision_idx
    ON moa.experiment_run_artifact_revision (revision_uid);

CREATE INDEX IF NOT EXISTS experiment_run_artifact_revision_scope_idx
    ON moa.experiment_run_artifact_revision (workspace_id, scope, user_id, revision_uid);

SELECT moa.apply_three_tier_rls('moa.experiment_run_artifact_revision'::REGCLASS);
