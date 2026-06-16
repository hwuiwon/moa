CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE IF NOT EXISTS moa.artifact (
    artifact_uid UUID PRIMARY KEY,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    tags TEXT[] NOT NULL DEFAULT ARRAY[]::TEXT[],
    latest_revision_uid UUID,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (kind IN ('skill', 'connector', 'workflow', 'action')),
    CHECK (name <> '')
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_active_name_uniq
    ON moa.artifact (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        kind,
        name
    )
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_scope_idx
    ON moa.artifact (workspace_id, scope, user_id, kind, name)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_tags_gin
    ON moa.artifact USING GIN (tags);

CREATE TABLE IF NOT EXISTS moa.artifact_revision (
    revision_uid UUID PRIMARY KEY,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    definition JSONB NOT NULL,
    canonical_hash BYTEA NOT NULL,
    source_format TEXT NOT NULL,
    source_text BYTEA NOT NULL,
    status TEXT NOT NULL,
    validation_report JSONB NOT NULL DEFAULT '{}'::JSONB,
    version INT NOT NULL,
    published_at TIMESTAMPTZ,
    valid_to TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (source_format IN ('json', 'yaml')),
    CHECK (status IN ('draft', 'published', 'archived')),
    CHECK (version > 0)
);

ALTER TABLE moa.artifact
    DROP CONSTRAINT IF EXISTS artifact_latest_revision_fk,
    ADD CONSTRAINT artifact_latest_revision_fk
        FOREIGN KEY (latest_revision_uid)
        REFERENCES moa.artifact_revision(revision_uid)
        DEFERRABLE INITIALLY DEFERRED;

CREATE UNIQUE INDEX IF NOT EXISTS artifact_revision_version_uniq
    ON moa.artifact_revision (artifact_uid, version);

CREATE INDEX IF NOT EXISTS artifact_revision_artifact_idx
    ON moa.artifact_revision (artifact_uid, status, version DESC)
    WHERE valid_to IS NULL;

CREATE INDEX IF NOT EXISTS artifact_revision_scope_idx
    ON moa.artifact_revision (workspace_id, scope, user_id, status)
    WHERE valid_to IS NULL;

CREATE TABLE IF NOT EXISTS moa.artifact_file (
    file_uid UUID PRIMARY KEY,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE CASCADE,
    revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    path TEXT NOT NULL,
    content BYTEA NOT NULL,
    content_sha256 BYTEA NOT NULL,
    content_type TEXT,
    executable BOOLEAN NOT NULL DEFAULT false,
    file_size_bytes BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (path <> ''),
    CHECK (path NOT LIKE '/%'),
    CHECK (path NOT LIKE '%..%'),
    CHECK (file_size_bytes >= 0)
);

CREATE UNIQUE INDEX IF NOT EXISTS artifact_file_revision_path_uniq
    ON moa.artifact_file (revision_uid, path);

CREATE INDEX IF NOT EXISTS artifact_file_artifact_idx
    ON moa.artifact_file (artifact_uid);

CREATE INDEX IF NOT EXISTS artifact_file_scope_idx
    ON moa.artifact_file (workspace_id, scope, user_id);

CREATE TABLE IF NOT EXISTS moa.artifact_run (
    run_uid UUID PRIMARY KEY,
    artifact_uid UUID REFERENCES moa.artifact(artifact_uid) ON DELETE SET NULL,
    revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE SET NULL,
    workspace_id TEXT,
    user_id TEXT,
    session_id UUID,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    workflow_ref TEXT NOT NULL,
    status TEXT NOT NULL,
    current_node_id TEXT,
    input JSONB NOT NULL DEFAULT '{}'::JSONB,
    state JSONB NOT NULL DEFAULT '{}'::JSONB,
    output JSONB,
    error TEXT,
    idempotency_key TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (status IN ('queued', 'running', 'waiting_approval', 'completed', 'failed', 'cancelled'))
);

CREATE INDEX IF NOT EXISTS artifact_run_scope_idx
    ON moa.artifact_run (workspace_id, scope, user_id, status, started_at DESC);

CREATE INDEX IF NOT EXISTS artifact_run_session_idx
    ON moa.artifact_run (session_id, started_at DESC)
    WHERE session_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS artifact_run_idempotency_uniq
    ON moa.artifact_run (
        coalesce(workspace_id, ''),
        coalesce(user_id, ''),
        workflow_ref,
        idempotency_key
    )
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS moa.artifact_node_run (
    node_run_uid UUID PRIMARY KEY,
    run_uid UUID NOT NULL REFERENCES moa.artifact_run(run_uid) ON DELETE CASCADE,
    workspace_id TEXT,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    node_id TEXT NOT NULL,
    status TEXT NOT NULL,
    input JSONB NOT NULL DEFAULT '{}'::JSONB,
    output JSONB,
    error TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (status IN ('queued', 'running', 'waiting_approval', 'completed', 'failed', 'cancelled', 'skipped'))
);

CREATE INDEX IF NOT EXISTS artifact_node_run_run_idx
    ON moa.artifact_node_run (run_uid, started_at ASC);

CREATE INDEX IF NOT EXISTS artifact_node_run_scope_idx
    ON moa.artifact_node_run (workspace_id, scope, user_id, status);

SELECT moa.apply_three_tier_rls('moa.artifact'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_revision'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_file'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_run'::REGCLASS);
SELECT moa.apply_three_tier_rls('moa.artifact_node_run'::REGCLASS);
