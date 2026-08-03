-- Tenant-owned connector connections, immutable compiled action bindings, and
-- replay-safe invocation records. Credential material remains in the standalone
-- auth-provider vault tables and is referenced only by logical slot name.

CREATE TABLE moa.connector_connections (
    connection_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    display_name TEXT NOT NULL,
    artifact_uid UUID,
    revision_uid UUID,
    built_in_key TEXT,
    built_in_version BIGINT,
    non_secret_config JSONB NOT NULL DEFAULT '{}'::JSONB,
    config_generation BIGINT NOT NULL DEFAULT 1,
    lifecycle_status TEXT NOT NULL DEFAULT 'pending_auth',
    health_status TEXT NOT NULL DEFAULT 'pending',
    health_reason TEXT,
    created_by_identity_id UUID,
    owner_identity_id UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT connector_connections_display_name_present CHECK (
        octet_length(display_name) BETWEEN 1 AND 512
        AND btrim(display_name) <> ''
    ),
    CONSTRAINT connector_connections_definition_exactly_one CHECK (
        (
            artifact_uid IS NOT NULL
            AND revision_uid IS NOT NULL
            AND built_in_key IS NULL
            AND built_in_version IS NULL
        )
        OR
        (
            artifact_uid IS NULL
            AND revision_uid IS NULL
            AND built_in_key IS NOT NULL
            AND btrim(built_in_key) <> ''
            AND built_in_version IS NOT NULL
            AND built_in_version > 0
        )
    ),
    CONSTRAINT connector_connections_config_object CHECK (
        jsonb_typeof(non_secret_config) = 'object'
    ),
    CONSTRAINT connector_connections_generation_positive CHECK (config_generation > 0),
    CONSTRAINT connector_connections_lifecycle_valid CHECK (
        lifecycle_status IN (
            'pending_auth', 'active', 'suspended', 'disconnecting', 'deleted'
        )
    ),
    CONSTRAINT connector_connections_health_valid CHECK (
        health_status IN ('pending', 'ready', 'degraded', 'unavailable', 'quarantined')
    ),
    CONSTRAINT connector_connections_health_reason_bounded CHECK (
        health_reason IS NULL
        OR (octet_length(health_reason) BETWEEN 1 AND 2048 AND btrim(health_reason) <> '')
    ),
    CONSTRAINT connector_connections_revision_fk
        FOREIGN KEY (revision_uid, artifact_uid)
        REFERENCES moa.artifact_revision (revision_uid, artifact_uid)
        ON DELETE RESTRICT,
    CONSTRAINT connector_connections_tenant_identity
        UNIQUE (connection_uid, tenant_id)
);

CREATE INDEX connector_connections_tenant_lifecycle_idx
    ON moa.connector_connections (tenant_id, lifecycle_status, connection_uid);

CREATE TABLE moa.connector_action_bindings (
    binding_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    connection_uid UUID NOT NULL,
    action_id TEXT NOT NULL,
    connection_generation BIGINT NOT NULL,
    compiled_contract JSONB NOT NULL,
    contract_hash TEXT NOT NULL,
    governed_contract_revision TEXT NOT NULL,
    minimum_effect TEXT NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT connector_action_bindings_connection_fk
        FOREIGN KEY (connection_uid, tenant_id)
        REFERENCES moa.connector_connections (connection_uid, tenant_id)
        ON DELETE CASCADE,
    CONSTRAINT connector_action_bindings_action_id_present CHECK (
        octet_length(action_id) BETWEEN 1 AND 255 AND btrim(action_id) <> ''
    ),
    CONSTRAINT connector_action_bindings_generation_positive CHECK (connection_generation > 0),
    CONSTRAINT connector_action_bindings_contract_object CHECK (
        jsonb_typeof(compiled_contract) = 'object'
    ),
    CONSTRAINT connector_action_bindings_contract_hash_valid CHECK (
        contract_hash ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT connector_action_bindings_revision_present CHECK (
        octet_length(governed_contract_revision) BETWEEN 1 AND 255
        AND btrim(governed_contract_revision) <> ''
    ),
    CONSTRAINT connector_action_bindings_minimum_effect_valid CHECK (
        minimum_effect IN ('allow', 'deny', 'admin_review')
    ),
    CONSTRAINT connector_action_bindings_generation_action_unique
        UNIQUE (connection_uid, connection_generation, action_id),
    CONSTRAINT connector_action_bindings_invocation_identity
        UNIQUE (binding_uid, tenant_id, connection_uid, connection_generation)
);

CREATE INDEX connector_action_bindings_tenant_catalog_idx
    ON moa.connector_action_bindings (tenant_id, enabled, connection_uid, action_id);

CREATE TABLE moa.connector_action_invocations (
    invocation_uid UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    connection_uid UUID NOT NULL,
    binding_uid UUID NOT NULL,
    connection_generation BIGINT NOT NULL,
    tool_call_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    upstream_idempotency_key TEXT,
    state TEXT NOT NULL DEFAULT 'reserved',
    error_metadata JSONB,
    output_metadata JSONB,
    started_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT connector_action_invocations_binding_fk
        FOREIGN KEY (binding_uid, tenant_id, connection_uid, connection_generation)
        REFERENCES moa.connector_action_bindings (
            binding_uid, tenant_id, connection_uid, connection_generation
        )
        ON DELETE RESTRICT,
    CONSTRAINT connector_action_invocations_tool_call_present CHECK (
        octet_length(tool_call_id) BETWEEN 1 AND 512 AND btrim(tool_call_id) <> ''
    ),
    CONSTRAINT connector_action_invocations_request_hash_valid CHECK (
        request_hash ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT connector_action_invocations_upstream_key_bounded CHECK (
        upstream_idempotency_key IS NULL
        OR (
            octet_length(upstream_idempotency_key) BETWEEN 1 AND 512
            AND btrim(upstream_idempotency_key) <> ''
        )
    ),
    CONSTRAINT connector_action_invocations_state_valid CHECK (
        state IN (
            'reserved', 'transmitting', 'succeeded', 'failed_before_send',
            'failed', 'unknown_outcome'
        )
    ),
    CONSTRAINT connector_action_invocations_error_object CHECK (
        error_metadata IS NULL OR jsonb_typeof(error_metadata) = 'object'
    ),
    CONSTRAINT connector_action_invocations_output_object CHECK (
        output_metadata IS NULL OR jsonb_typeof(output_metadata) = 'object'
    ),
    CONSTRAINT connector_action_invocations_terminal_time CHECK (
        (state IN ('reserved', 'transmitting') AND completed_at IS NULL)
        OR (
            state IN ('succeeded', 'failed_before_send', 'failed', 'unknown_outcome')
            AND completed_at IS NOT NULL
        )
    ),
    CONSTRAINT connector_action_invocations_replay_key
        UNIQUE (tenant_id, tool_call_id)
);

CREATE INDEX connector_action_invocations_connection_idx
    ON moa.connector_action_invocations (
        tenant_id, connection_uid, connection_generation, started_at DESC
    );

-- Reservation commits replay ownership before the transport boundary. Moving to
-- `transmitting` is the one durable claim that authorizes the first send; an
-- identical replay reads that state rather than updating it or sending again.
CREATE FUNCTION moa.enforce_connector_action_invocation_transition() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state <> 'reserved' THEN
            RAISE EXCEPTION 'connector action invocation must be inserted as reserved'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'connector_action_invocations_state_transition_valid';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.invocation_uid IS DISTINCT FROM OLD.invocation_uid
       OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.connection_uid IS DISTINCT FROM OLD.connection_uid
       OR NEW.binding_uid IS DISTINCT FROM OLD.binding_uid
       OR NEW.connection_generation IS DISTINCT FROM OLD.connection_generation
       OR NEW.tool_call_id IS DISTINCT FROM OLD.tool_call_id
       OR NEW.request_hash IS DISTINCT FROM OLD.request_hash
       OR NEW.started_at IS DISTINCT FROM OLD.started_at THEN
        RAISE EXCEPTION 'connector action invocation identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'connector_action_invocations_identity_immutable';
    END IF;

    IF OLD.state IN ('succeeded', 'failed_before_send', 'failed', 'unknown_outcome') THEN
        RAISE EXCEPTION 'terminal connector action invocation is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'connector_action_invocations_terminal_immutable';
    END IF;

    transition_allowed := CASE OLD.state
        WHEN 'reserved' THEN NEW.state IN ('transmitting', 'failed_before_send')
        WHEN 'transmitting' THEN NEW.state IN ('succeeded', 'failed', 'unknown_outcome')
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid connector action invocation transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '23514',
                  CONSTRAINT = 'connector_action_invocations_state_transition_valid';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER connector_action_invocation_transition_guard
BEFORE INSERT OR UPDATE ON moa.connector_action_invocations
FOR EACH ROW EXECUTE FUNCTION moa.enforce_connector_action_invocation_transition();

-- BEGIN TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT
ALTER TABLE tenant_credential_versions
    ADD COLUMN IF NOT EXISTS slot_name TEXT NOT NULL DEFAULT 'primary';
ALTER TABLE tenant_credential_versions
    DROP CONSTRAINT IF EXISTS tenant_credential_versions_slot_name_valid;
ALTER TABLE tenant_credential_versions
    ADD CONSTRAINT tenant_credential_versions_slot_name_valid
        CHECK (slot_name ~ '^[a-z][a-z0-9_]{0,62}$');
ALTER TABLE tenant_credential_versions
    DROP CONSTRAINT IF EXISTS tenant_credential_versions_identity_key;
ALTER TABLE tenant_credential_versions
    ADD CONSTRAINT tenant_credential_versions_identity_key
        UNIQUE (tenant_id, connection_uid, kind, slot_name, version);
DROP INDEX IF EXISTS tenant_credential_versions_one_active;
CREATE UNIQUE INDEX tenant_credential_versions_one_active
    ON tenant_credential_versions (tenant_id, connection_uid, kind, slot_name)
    WHERE active;

ALTER TABLE tenant_credential_operations
    ADD COLUMN IF NOT EXISTS slot_name TEXT NOT NULL DEFAULT 'primary';
ALTER TABLE tenant_credential_operations
    DROP CONSTRAINT IF EXISTS tenant_credential_operations_slot_name_valid;
ALTER TABLE tenant_credential_operations
    ADD CONSTRAINT tenant_credential_operations_slot_name_valid
        CHECK (slot_name ~ '^[a-z][a-z0-9_]{0,62}$');
-- END TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT

-- Rollout-compatible parents for existing Nango/Merge knowledge projections.
-- V51 adds the child foreign key only after all old writers create this parent.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.knowledge_connections
        WHERE provider NOT IN ('nango', 'merge')
    ) THEN
        RAISE EXCEPTION 'knowledge connection provider has no closed connector parent mapping'
            USING ERRCODE = '23514';
    END IF;
END
$$;

INSERT INTO moa.connector_connections (
    connection_uid,
    tenant_id,
    display_name,
    built_in_key,
    built_in_version,
    non_secret_config,
    config_generation,
    lifecycle_status,
    health_status,
    created_at,
    updated_at
)
SELECT
    connection.connection_uid,
    connection.tenant_id,
    connection.connector,
    'knowledge:' || connection.provider,
    1,
    jsonb_build_object(
        'provider_config_key', connection.provider_config_key,
        'provider_connection_id', connection.provider_connection_id,
        'connector', connection.connector,
        'source_selection', connection.source_selection
    ),
    1,
    CASE connection.status
        WHEN 'pending' THEN 'pending_auth'
        WHEN 'active' THEN 'active'
        ELSE 'suspended'
    END,
    CASE connection.status
        WHEN 'pending' THEN 'pending'
        WHEN 'active' THEN 'ready'
        WHEN 'error' THEN 'degraded'
        ELSE 'unavailable'
    END,
    connection.created_at,
    connection.updated_at
FROM moa.knowledge_connections AS connection;

-- These connector tables are always tenant-owned. A missing or wrong tenant GUC
-- denies rather than opening a control-plane branch. RLS is forced only after
-- the rollout backfill so the migration owner can create legacy parents.
ALTER TABLE moa.connector_connections ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_connections FORCE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_action_bindings ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_action_bindings FORCE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_action_invocations ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_action_invocations FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.connector_connections FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);
CREATE POLICY tenant_isolation ON moa.connector_action_bindings FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);
CREATE POLICY rd_tenant ON moa.connector_action_invocations FOR SELECT TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);
CREATE POLICY wr_tenant ON moa.connector_action_invocations FOR INSERT TO moa_app
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);
CREATE POLICY up_tenant ON moa.connector_action_invocations FOR UPDATE TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.connector_connections TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.connector_action_bindings TO moa_app;
-- Invocation audit can transition to a terminal state, but ordinary callers
-- never delete it. The owner-only bounded tenant purge is the destructive path.
GRANT SELECT, INSERT, UPDATE ON moa.connector_action_invocations TO moa_app;

-- Insert connector children immediately after knowledge rows and before every
-- artifact row they can reference. Moving the existing order through a remote
-- key range avoids transient primary-key collisions.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= 26;
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 997
WHERE stage_order >= 1026;

INSERT INTO moa.tenant_purge_catalog
    (stage_order, stage_name, table_schema, table_name, scope_mode, action_mode)
VALUES
    (26, 'moa.connector_action_invocations', 'moa', 'connector_action_invocations', 'tenant_id', 'delete'),
    (27, 'moa.connector_action_bindings', 'moa', 'connector_action_bindings', 'tenant_id', 'delete'),
    (28, 'moa.connector_connections', 'moa', 'connector_connections', 'tenant_id', 'delete');

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 130-table tenant-offboarding residue surface. The two nullable-scope simulator certification authority tables are intentionally global and absent.';

-- New tables were not present when V48 installed its catalog-derived statement
-- fences, so attach the same typed tenant fence now.
CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.connector_connections
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.connector_connections
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.connector_action_bindings
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.connector_action_bindings
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.connector_action_invocations
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.connector_action_invocations
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');

-- V48's bounded function intentionally pins the exact catalog cardinality.
-- Replace that definition from its canonical stored source while asserting the
-- expected predecessor text, so a drifted predecessor fails migration instead
-- of silently weakening the final residue proof.
DO $tenant_connector_purge$
DECLARE
    predecessor TEXT;
    replacement TEXT;
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 127%'
       OR predecessor NOT LIKE '%exactly 127 tables%'
    THEN
        RAISE EXCEPTION 'unexpected V48 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, 'catalog_count <> 127', 'catalog_count <> 130');
    replacement := replace(replacement, 'exactly 127 tables', 'exactly 130 tables');
    EXECUTE replacement;
END
$tenant_connector_purge$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;
