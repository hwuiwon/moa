-- Tenant-configurable agent artifacts, deployment pointers, and session pins.

DROP TABLE IF EXISTS moa.skill_file CASCADE;
DROP TABLE IF EXISTS moa.skill_addendum CASCADE;
DROP TABLE IF EXISTS moa.skill CASCADE;

DROP POLICY IF EXISTS rd_auditor ON moa.artifact;
CREATE POLICY rd_auditor ON moa.artifact
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.artifact_revision;
CREATE POLICY rd_auditor ON moa.artifact_revision
    FOR SELECT TO moa_auditor
    USING (true);

DROP POLICY IF EXISTS rd_auditor ON moa.artifact_file;
CREATE POLICY rd_auditor ON moa.artifact_file
    FOR SELECT TO moa_auditor
    USING (true);

GRANT SELECT ON moa.artifact TO moa_auditor;
GRANT SELECT ON moa.artifact_revision TO moa_auditor;
GRANT SELECT ON moa.artifact_file TO moa_auditor;

ALTER TABLE moa.artifact
    DROP CONSTRAINT IF EXISTS artifact_kind_check;

ALTER TABLE moa.artifact
    ADD CONSTRAINT artifact_kind_check CHECK (
        kind IN (
            'agent',
            'skill',
            'connector',
            'action',
            'experiment_plan'
        )
    );

INSERT INTO moa.artifact (
    artifact_uid, storage_partition_id, user_id, kind, name, description, tags, latest_revision_uid
)
VALUES (
    '00000000-0000-4000-8000-000000000a01',
    NULL,
    NULL,
    'agent',
    'system-default',
    'Built-in default agent used for internal and fixture sessions that do not select a tenant-authored agent.',
    ARRAY['system', 'default'],
    NULL
)
ON CONFLICT (artifact_uid) DO NOTHING;

INSERT INTO moa.artifact_revision (
    revision_uid, artifact_uid, storage_partition_id, user_id, definition,
    canonical_hash, source_format, source_text, status, validation_report,
    version, published_at
)
VALUES (
    '00000000-0000-4000-8000-000000000a02',
    '00000000-0000-4000-8000-000000000a01',
    NULL,
    NULL,
    '{
      "api_version": "moa.artifact/v1",
      "kind": "agent",
      "metadata": {
        "name": "system-default",
        "description": "Built-in default agent",
        "tags": ["system", "default"]
      },
      "definition": {
        "type": "agent",
        "spec": {
          "display_name": "MOA Default Agent",
          "purpose": {
            "summary": "Default agent policy for sessions that have no tenant-authored agent.",
            "expected_outputs": []
          },
          "instruction_policy": {
            "instructions": []
          },
          "tool_policy": {
            "mode": "auto",
            "tools": [],
            "denied_tools": []
          }
        }
      }
    }'::JSONB,
    decode('b2d1f753116dfa8c9ca4f2a07c83d61cb7dd99fb9f24daf3f47ecae520b191ed', 'hex'),
    'json',
    convert_to('{"kind":"agent","name":"system-default"}', 'UTF8'),
    'published',
    '{"ok": true}'::JSONB,
    1,
    now()
)
ON CONFLICT (revision_uid) DO NOTHING;

UPDATE moa.artifact
SET latest_revision_uid = '00000000-0000-4000-8000-000000000a02',
    updated_at = now()
WHERE artifact_uid = '00000000-0000-4000-8000-000000000a01'
  AND latest_revision_uid IS NULL;

CREATE TABLE IF NOT EXISTS moa.agent_installation (
    installation_uid UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    agent_id UUID,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE RESTRICT,
    definition_ref TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'inactive', 'retired')),
    current_revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    staged_revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    last_deployment_uid UUID,
    last_deployed_at TIMESTAMPTZ,
    installed_by TEXT,
    deployment_metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (definition_ref <> '')
);

CREATE INDEX IF NOT EXISTS agent_installation_storage_partition_status_idx
    ON moa.agent_installation (storage_partition_id, status, updated_at DESC);

CREATE INDEX IF NOT EXISTS agent_installation_current_revision_idx
    ON moa.agent_installation (current_revision_uid)
    WHERE current_revision_uid IS NOT NULL;

CREATE INDEX IF NOT EXISTS agent_installation_artifact_idx
    ON moa.agent_installation (artifact_uid, storage_partition_id, status);

CREATE UNIQUE INDEX IF NOT EXISTS agent_installation_active_ref_uniq
    ON moa.agent_installation (
        storage_partition_id,
        coalesce(user_id, ''),
        definition_ref
    )
    WHERE status <> 'retired';

SELECT moa.apply_three_tier_rls('moa.agent_installation'::REGCLASS);

CREATE TABLE IF NOT EXISTS moa.agent_deployment (
    deployment_uid UUID PRIMARY KEY,
    installation_uid UUID NOT NULL REFERENCES moa.agent_installation(installation_uid) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    deployed_by TEXT,
    deployed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'superseded', 'rolled_back', 'retired')),
    reason TEXT,
    dependency_lock JSONB NOT NULL DEFAULT '{}'::JSONB,
    dependency_lock_hash TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS agent_deployment_installation_idx
    ON moa.agent_deployment (installation_uid, deployed_at DESC);

CREATE INDEX IF NOT EXISTS agent_deployment_revision_idx
    ON moa.agent_deployment (revision_uid, deployed_at DESC);

CREATE INDEX IF NOT EXISTS agent_deployment_scope_idx
    ON moa.agent_deployment (storage_partition_id, scope, user_id, status);

SELECT moa.apply_three_tier_rls('moa.agent_deployment'::REGCLASS);

ALTER TABLE sessions
    DROP CONSTRAINT IF EXISTS sessions_user_id_nonempty;

ALTER TABLE sessions
    ADD CONSTRAINT sessions_user_id_nonempty CHECK (btrim(user_id) <> '');

CREATE TABLE IF NOT EXISTS session_agent_context (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    agent_id UUID,
    installation_uid UUID REFERENCES moa.agent_installation(installation_uid) ON DELETE SET NULL,
    deployment_uid UUID REFERENCES moa.agent_deployment(deployment_uid) ON DELETE SET NULL,
    agent_definition_ref TEXT NOT NULL,
    agent_revision_uid UUID NOT NULL REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    policy_hash TEXT NOT NULL,
    display_name TEXT NOT NULL,
    policy_snapshot JSONB NOT NULL,
    artifact_dependencies JSONB NOT NULL DEFAULT '[]'::JSONB,
    tool_dependencies JSONB NOT NULL DEFAULT '[]'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (btrim(user_id) <> ''),
    CHECK (agent_definition_ref <> ''),
    CHECK (policy_hash <> ''),
    CHECK (display_name <> '')
);

ALTER TABLE session_agent_context
    ALTER COLUMN user_id SET NOT NULL;

ALTER TABLE session_agent_context
    DROP CONSTRAINT IF EXISTS session_agent_context_user_id_nonempty;

ALTER TABLE session_agent_context
    ADD CONSTRAINT session_agent_context_user_id_nonempty CHECK (btrim(user_id) <> '');

INSERT INTO session_agent_context (
    session_id, storage_partition_id, user_id, agent_definition_ref, agent_revision_uid,
    policy_hash, display_name, policy_snapshot, artifact_dependencies, tool_dependencies
)
SELECT
    sessions.id,
    sessions.storage_partition_id,
    sessions.user_id,
    'agent://system-default',
    '00000000-0000-4000-8000-000000000a02',
    'system-default-agent-v1',
    'MOA Default Agent',
    jsonb_build_object(
        'instructions', '[]'::JSONB,
        'tool_policy', jsonb_build_object(
            'mode', 'auto',
            'tools', '[]'::JSONB,
            'denied_tools', '[]'::JSONB
        ),
        'revision_lock', jsonb_build_object(
            'agent_revision_uid', '00000000-0000-4000-8000-000000000a02',
            'artifact_dependencies', '[]'::JSONB,
            'tool_dependencies', '[]'::JSONB,
            'canonical_policy_hash', 'system-default-agent-v1'
        )
    ),
    '[]'::JSONB,
    '[]'::JSONB
FROM sessions
ON CONFLICT (session_id) DO NOTHING;

CREATE INDEX IF NOT EXISTS session_agent_context_revision_idx
    ON session_agent_context (agent_revision_uid);

CREATE INDEX IF NOT EXISTS session_agent_context_scope_idx
    ON session_agent_context (storage_partition_id, scope, user_id, agent_revision_uid);

SELECT moa.apply_three_tier_rls('session_agent_context'::REGCLASS);

CREATE INDEX IF NOT EXISTS node_index_lesson_skill_uid_idx
    ON moa.node_index ((properties_summary->>'skill_uid'), valid_from DESC)
    WHERE label = 'Lesson'
      AND valid_to IS NULL;

CREATE OR REPLACE FUNCTION moa.require_session_agent_context()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, public
AS $$
DECLARE
    has_context BOOLEAN;
BEGIN
    EXECUTE format(
        'SELECT EXISTS (SELECT 1 FROM %I.session_agent_context WHERE session_id = $1)',
        TG_TABLE_SCHEMA
    )
    INTO has_context
    USING NEW.id;

    IF NOT has_context THEN
        RAISE EXCEPTION 'session % is missing required agent context', NEW.id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS session_requires_agent_context ON sessions;

CREATE CONSTRAINT TRIGGER session_requires_agent_context
    AFTER INSERT OR UPDATE OF id ON sessions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION moa.require_session_agent_context();
