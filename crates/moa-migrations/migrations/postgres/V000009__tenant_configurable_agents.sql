-- Tenant-configurable agent artifacts, deployment pointers, and session pins.

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
    'superseded',
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
    tenant_id UUID NOT NULL,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    agent_id UUID,
    artifact_uid UUID NOT NULL REFERENCES moa.artifact(artifact_uid) ON DELETE RESTRICT,
    definition_ref TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active'
        CHECK (status IN ('active', 'inactive', 'retired')),
    current_revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    staged_revision_uid UUID REFERENCES moa.artifact_revision(revision_uid) ON DELETE RESTRICT,
    serving_pointer_version BIGINT NOT NULL DEFAULT 0,
    activation_attestation_uid UUID,
    last_deployment_uid UUID,
    last_deployed_at TIMESTAMPTZ,
    installed_by TEXT,
    deployment_metadata JSONB NOT NULL DEFAULT '{}'::JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (scope IS NOT NULL),
    CHECK (definition_ref <> ''),
    CONSTRAINT agent_installation_pointer_version_nonnegative
        CHECK (serving_pointer_version >= 0)
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

CREATE TABLE IF NOT EXISTS moa.agent_deployment (
    deployment_uid UUID PRIMARY KEY,
    installation_uid UUID NOT NULL REFERENCES moa.agent_installation(installation_uid) ON DELETE CASCADE,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    tenant_id UUID NOT NULL,
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

CREATE TABLE IF NOT EXISTS session_agent_context (
    session_id UUID PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    tenant_id UUID NOT NULL,
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
    CONSTRAINT session_agent_context_user_id_nonempty CHECK (btrim(user_id) <> ''),
    CHECK (agent_definition_ref <> ''),
    CHECK (policy_hash <> ''),
    CHECK (display_name <> '')
);

ALTER TABLE session_agent_context
    ADD CONSTRAINT session_agent_context_session_tenant_fkey
    FOREIGN KEY (session_id, tenant_id)
    REFERENCES sessions(id, tenant_id)
    ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS session_agent_context_revision_idx
    ON session_agent_context (agent_revision_uid);

CREATE INDEX IF NOT EXISTS session_agent_context_scope_idx
    ON session_agent_context (storage_partition_id, scope, user_id, agent_revision_uid);

CREATE INDEX IF NOT EXISTS node_index_lesson_skill_uid_idx
    ON moa.node_index ((properties_summary->>'skill_uid'), valid_from DESC)
    WHERE label = 'Lesson'
      AND valid_to IS NULL;

CREATE OR REPLACE FUNCTION moa.require_session_agent_context()
RETURNS TRIGGER
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    has_context BOOLEAN;
BEGIN
    IF TG_TABLE_SCHEMA <> 'public' OR TG_TABLE_NAME <> 'sessions' THEN
        RAISE EXCEPTION 'moa.require_session_agent_context may only guard public.sessions'
            USING ERRCODE = '42501';
    END IF;

    SELECT EXISTS (
        SELECT 1
        FROM public.session_agent_context context
        WHERE context.session_id = NEW.id
          AND context.tenant_id = NEW.tenant_id
    ) INTO has_context;

    IF NOT has_context THEN
        RAISE EXCEPTION 'session % is missing required agent context', NEW.id
            USING ERRCODE = '23514';
    END IF;

    RETURN NEW;
END;
$$;

ALTER FUNCTION moa.require_session_agent_context() OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.require_session_agent_context() FROM PUBLIC;

CREATE CONSTRAINT TRIGGER session_requires_agent_context
    AFTER INSERT OR UPDATE OF id ON public.sessions
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION moa.require_session_agent_context();
