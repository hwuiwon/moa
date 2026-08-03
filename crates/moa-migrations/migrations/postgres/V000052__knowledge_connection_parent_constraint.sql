-- Make the generic connector row the tenant-exact lifecycle parent for every
-- knowledge connection, and add the two replay ledgers needed by managed parent
-- claims and provider disconnects.

-- V50 backfilled the knowledge rows that existed at its rollout boundary. A
-- mixed-version rollout could still have admitted more knowledge projections
-- before every writer learned to create the shared connector parent. Keep this
-- catch-up deliberately closed to the two code-owned provider definitions.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.knowledge_connections
        WHERE provider NOT IN ('nango', 'merge')
    ) THEN
        RAISE EXCEPTION 'knowledge connection provider has no closed connector parent mapping'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connections_provider_parent_mapping_valid';
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
FROM moa.knowledge_connections AS connection
WHERE NOT EXISTS (
    SELECT 1
    FROM moa.connector_connections AS parent
    WHERE parent.connection_uid = connection.connection_uid
);

-- A pre-existing UUID is compatible only when it already names the exact
-- tenant, built-in provider definition, and provider-native account identity.
-- Mutable lifecycle, health, display name, generation, and source selection
-- remain parent/projection state and are intentionally not overwritten here.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.knowledge_connections AS connection
        JOIN moa.connector_connections AS parent
          ON parent.connection_uid = connection.connection_uid
        WHERE parent.tenant_id IS DISTINCT FROM connection.tenant_id
           OR parent.artifact_uid IS NOT NULL
           OR parent.revision_uid IS NOT NULL
           OR parent.built_in_key IS DISTINCT FROM 'knowledge:' || connection.provider
           OR parent.built_in_version IS DISTINCT FROM 1
           OR parent.non_secret_config ->> 'provider_config_key'
                IS DISTINCT FROM connection.provider_config_key
           OR parent.non_secret_config ->> 'provider_connection_id'
                IS DISTINCT FROM connection.provider_connection_id
           OR parent.non_secret_config ->> 'connector'
                IS DISTINCT FROM connection.connector
    ) THEN
        RAISE EXCEPTION 'knowledge connection has an incompatible connector parent'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connections_parent_identity_compatible';
    END IF;
END
$$;

-- The tuple table is latest desired state rather than append-only history. This
-- mirrors the runtime enqueue operation: a matching write is a no-op, while a
-- stale delete/dead-letter is deterministically restored and re-queued.
INSERT INTO public.authz_outbox (
    op,
    tuple_user,
    tuple_relation,
    tuple_object,
    model_version,
    tenant_id,
    generation,
    status,
    attempts,
    next_attempt_at
)
SELECT
    'write',
    'tenant:' || connection.tenant_id::TEXT,
    'tenant',
    'connector_connection:' || connection.connection_uid::TEXT,
    6,
    connection.tenant_id,
    1,
    'pending',
    0,
    NOW()
FROM moa.knowledge_connections AS connection
ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE
SET op = EXCLUDED.op,
    tenant_id = EXCLUDED.tenant_id,
    generation = authz_outbox.generation + 1,
    status = 'pending',
    attempts = 0,
    last_error = NULL,
    lease_token = NULL,
    lease_expires_at = NULL,
    next_attempt_at = NOW(),
    updated_at = NOW()
WHERE authz_outbox.tenant_id IS DISTINCT FROM EXCLUDED.tenant_id
   OR authz_outbox.op IS DISTINCT FROM EXCLUDED.op
   OR authz_outbox.status = 'dead_letter';

-- Prove the catch-up is complete before making parent presence a permanent
-- write-time invariant. The composite key prevents a globally unique UUID from
-- accidentally masking a tenant mismatch.
DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM moa.knowledge_connections AS connection
        LEFT JOIN moa.connector_connections AS parent
          ON parent.connection_uid = connection.connection_uid
         AND parent.tenant_id = connection.tenant_id
        WHERE parent.connection_uid IS NULL
    ) THEN
        RAISE EXCEPTION 'knowledge connection is missing its same-tenant connector parent'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connections_parent_orphan_free';
    END IF;
END
$$;

-- The generic parent now owns lifecycle and credential state. The knowledge
-- child is only the provider-specific capability projection, so the legacy
-- child-local credential locator and lifecycle columns must not survive the
-- final fresh-install schema.
ALTER TABLE moa.knowledge_connections
    DROP COLUMN credential_ref,
    DROP COLUMN status;

ALTER TABLE moa.knowledge_connections
    ADD CONSTRAINT knowledge_connections_tenant_identity
    UNIQUE (connection_uid, tenant_id);

ALTER TABLE moa.knowledge_connections
    ADD CONSTRAINT knowledge_connections_connector_parent_fk
    FOREIGN KEY (connection_uid, tenant_id)
    REFERENCES moa.connector_connections (connection_uid, tenant_id)
    ON DELETE RESTRICT;

-- A managed parent claim records ownership in the same transaction that either
-- creates the parent or proves an exact replay. The stored ownership bit is the
-- only authority for later compensation; parent existence is never evidence
-- that this operation created it.
CREATE TABLE moa.connector_managed_parent_claims (
    tenant_id UUID NOT NULL,
    operation_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    connection_uid UUID NOT NULL,
    parent_created_by_claim BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT connector_managed_parent_claims_pkey
        PRIMARY KEY (tenant_id, operation_id),
    CONSTRAINT connector_managed_parent_claims_operation_id_valid CHECK (
        octet_length(operation_id) BETWEEN 1 AND 512
        AND btrim(operation_id) <> ''
    ),
    CONSTRAINT connector_managed_parent_claims_request_hash_valid CHECK (
        request_hash ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT connector_managed_parent_claims_timestamps_valid CHECK (
        updated_at >= created_at
    ),
    CONSTRAINT connector_managed_parent_claims_connection_fk
        FOREIGN KEY (connection_uid, tenant_id)
        REFERENCES moa.connector_connections (connection_uid, tenant_id)
        ON DELETE RESTRICT
);

CREATE INDEX connector_managed_parent_claims_connection_idx
    ON moa.connector_managed_parent_claims (tenant_id, connection_uid);

CREATE FUNCTION moa.reject_connector_managed_parent_claim_update() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'connector managed parent claim is immutable'
        USING ERRCODE = '23514',
              CONSTRAINT = 'connector_managed_parent_claims_immutable';
END;
$$;

CREATE TRIGGER connector_managed_parent_claim_immutable_guard
BEFORE UPDATE ON moa.connector_managed_parent_claims
FOR EACH ROW EXECUTE FUNCTION moa.reject_connector_managed_parent_claim_update();

ALTER TABLE moa.connector_managed_parent_claims ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_managed_parent_claims FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.connector_managed_parent_claims
FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

-- Claim replay serializes on `SELECT ... FOR UPDATE`; PostgreSQL requires the
-- table-level UPDATE privilege for that row lock even though the immutable
-- trigger rejects every actual UPDATE statement.
GRANT SELECT, INSERT, UPDATE ON moa.connector_managed_parent_claims TO moa_app;

-- Extend the existing link claim without guessing an ownership bit or connector
-- generation for historical rows. A new flow records the parent phase, then
-- pins the exact expected generation before staging any credential operation.
ALTER TABLE moa.knowledge_link_claims
    DROP COLUMN previous_credential_ref,
    ADD COLUMN parent_created_by_claim BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN credential_expected_generation BIGINT,
    ADD COLUMN credential_ownership TEXT,
    ADD COLUMN previous_vault_credential_ref TEXT;

ALTER TABLE moa.knowledge_link_claims
    DROP CONSTRAINT knowledge_link_claims_state_valid,
    DROP CONSTRAINT knowledge_link_claims_candidate_recorded;
ALTER TABLE moa.knowledge_link_claims
    ADD CONSTRAINT knowledge_link_claims_state_valid
        CHECK (state IN (
            'reserved',
            'parent_claimed',
            'credential_written',
            'compensating',
            'compensated',
            'finalized'
        )),
    ADD CONSTRAINT knowledge_link_claims_credential_generation_positive
        CHECK (
            credential_expected_generation IS NULL
            OR credential_expected_generation > 0
        ),
    ADD CONSTRAINT knowledge_link_claims_parent_generation_recorded
        CHECK (
            state <> 'parent_claimed'
            OR credential_expected_generation IS NOT NULL
        ),
    ADD CONSTRAINT knowledge_link_claims_credential_ownership_valid
        CHECK (
            credential_ownership IS NULL
            OR credential_ownership IN ('provider_native', 'moa_managed')
        ),
    ADD CONSTRAINT knowledge_link_claims_candidate_vault_ref_bounded
        CHECK (
            credential_ownership IS NULL
            OR candidate_credential_ref IS NULL
            OR char_length(candidate_credential_ref) BETWEEN 1 AND 128
        ),
    ADD CONSTRAINT knowledge_link_claims_previous_vault_ref_bounded
        CHECK (
            previous_vault_credential_ref IS NULL
            OR char_length(previous_vault_credential_ref) BETWEEN 1 AND 128
        ),
    ADD CONSTRAINT knowledge_link_claims_credential_receipts_valid
        CHECK (
            credential_ownership IS NULL
            OR (
                credential_ownership = 'provider_native'
                AND candidate_credential_ref IS NULL
                AND previous_vault_credential_ref IS NULL
            )
            OR (
                credential_ownership = 'moa_managed'
                AND candidate_credential_ref IS NOT NULL
            )
        ),
    ADD CONSTRAINT knowledge_link_claims_candidate_recorded
        CHECK (
            state NOT IN ('credential_written', 'finalized')
            OR credential_ownership IS NULL
            OR credential_ownership = 'provider_native'
            OR candidate_credential_ref IS NOT NULL
        ),
    ADD CONSTRAINT knowledge_link_claims_prior_vault_owner_valid
        CHECK (
            previous_vault_credential_ref IS NULL
            OR credential_ownership = 'moa_managed'
        );

-- A connection can have many completed link attempts, but only one attempt may
-- own its parent/generation/credential compensation boundary at a time. This
-- prevents one relink from restoring or reactivating state underneath another
-- in-flight relink for the same shared parent.
CREATE UNIQUE INDEX knowledge_link_claims_one_nonterminal_per_connection
    ON moa.knowledge_link_claims (tenant_id, connection_uid)
    WHERE state NOT IN ('finalized', 'compensated');

CREATE FUNCTION moa.enforce_knowledge_link_claim_generation_immutable() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.credential_expected_generation IS NOT NULL
       AND NEW.credential_expected_generation
            IS DISTINCT FROM OLD.credential_expected_generation
    THEN
        RAISE EXCEPTION 'knowledge link claim credential generation is immutable once recorded'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_link_claims_credential_generation_immutable';
    END IF;
    IF OLD.credential_ownership IS NOT NULL
       AND NEW.credential_ownership IS DISTINCT FROM OLD.credential_ownership
    THEN
        RAISE EXCEPTION 'knowledge link claim credential ownership is immutable once recorded'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_link_claims_credential_ownership_immutable';
    END IF;
    IF OLD.candidate_credential_ref IS NOT NULL
       AND NEW.candidate_credential_ref IS DISTINCT FROM OLD.candidate_credential_ref
    THEN
        RAISE EXCEPTION 'knowledge link claim candidate vault receipt is immutable once recorded'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_link_claims_candidate_vault_ref_immutable';
    END IF;
    IF OLD.previous_vault_credential_ref IS NOT NULL
       AND NEW.previous_vault_credential_ref
            IS DISTINCT FROM OLD.previous_vault_credential_ref
    THEN
        RAISE EXCEPTION 'knowledge link claim prior vault receipt is immutable once recorded'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_link_claims_previous_vault_ref_immutable';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER knowledge_link_claim_generation_immutable_guard
BEFORE UPDATE ON moa.knowledge_link_claims
FOR EACH ROW EXECUTE FUNCTION moa.enforce_knowledge_link_claim_generation_immutable();

-- One durable provider-send ledger exists for the lifetime of a knowledge
-- connection. `transmitting` is the one-way send claim; an unknown outcome is
-- terminal for automatic retry and leaves the generic parent Disconnecting.
CREATE TABLE moa.knowledge_connection_disconnect_progress (
    tenant_id UUID NOT NULL,
    connection_uid UUID NOT NULL,
    operation_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    provider_operation_id UUID NOT NULL,
    state TEXT NOT NULL DEFAULT 'reserved',
    error_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    CONSTRAINT knowledge_connection_disconnect_progress_pkey
        PRIMARY KEY (tenant_id, connection_uid),
    CONSTRAINT knowledge_connection_disconnect_progress_operation_key
        UNIQUE (tenant_id, operation_id),
    CONSTRAINT knowledge_connection_disconnect_progress_operation_id_valid CHECK (
        octet_length(operation_id) BETWEEN 1 AND 512
        AND btrim(operation_id) <> ''
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_request_hash_valid CHECK (
        request_hash ~ '^[0-9a-f]{64}$'
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_state_valid CHECK (
        state IN (
            'reserved',
            'transmitting',
            'deleted',
            'already_absent',
            'failed_before_send',
            'unknown_outcome'
        )
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_error_code_valid CHECK (
        error_code IS NULL OR error_code ~ '^[a-z][a-z0-9_]{0,62}$'
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_outcome_valid CHECK (
        (
            state IN ('reserved', 'transmitting')
            AND completed_at IS NULL
            AND error_code IS NULL
        )
        OR (
            state IN ('deleted', 'already_absent')
            AND completed_at IS NOT NULL
            AND error_code IS NULL
        )
        OR (
            state IN ('failed_before_send', 'unknown_outcome')
            AND completed_at IS NOT NULL
            AND error_code IS NOT NULL
        )
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_timestamps_valid CHECK (
        updated_at >= created_at
        AND (completed_at IS NULL OR completed_at >= created_at)
    ),
    CONSTRAINT knowledge_connection_disconnect_progress_connection_fk
        FOREIGN KEY (connection_uid, tenant_id)
        REFERENCES moa.knowledge_connections (connection_uid, tenant_id)
        ON DELETE RESTRICT
);

CREATE FUNCTION moa.enforce_knowledge_connection_disconnect_transition() RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    transition_allowed BOOLEAN;
BEGIN
    IF TG_OP = 'INSERT' THEN
        IF NEW.state <> 'reserved' THEN
            RAISE EXCEPTION 'knowledge connection disconnect must be inserted as reserved'
                USING ERRCODE = '23514',
                      CONSTRAINT = 'knowledge_connection_disconnect_progress_transition_valid';
        END IF;
        RETURN NEW;
    END IF;

    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.connection_uid IS DISTINCT FROM OLD.connection_uid
       OR NEW.operation_id IS DISTINCT FROM OLD.operation_id
       OR NEW.request_hash IS DISTINCT FROM OLD.request_hash
       OR NEW.provider_operation_id IS DISTINCT FROM OLD.provider_operation_id
       OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION 'knowledge connection disconnect identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connection_disconnect_progress_identity_immutable';
    END IF;

    IF OLD.state IN (
        'deleted', 'already_absent', 'failed_before_send', 'unknown_outcome'
    ) THEN
        RAISE EXCEPTION 'terminal knowledge connection disconnect is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connection_disconnect_progress_terminal_immutable';
    END IF;

    transition_allowed := CASE OLD.state
        WHEN 'reserved' THEN NEW.state IN ('transmitting', 'failed_before_send')
        WHEN 'transmitting' THEN NEW.state IN ('deleted', 'already_absent', 'unknown_outcome')
        ELSE FALSE
    END;
    IF NOT transition_allowed THEN
        RAISE EXCEPTION 'invalid knowledge connection disconnect transition: % -> %',
            OLD.state, NEW.state
            USING ERRCODE = '23514',
                  CONSTRAINT = 'knowledge_connection_disconnect_progress_transition_valid';
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER knowledge_connection_disconnect_transition_guard
BEFORE INSERT OR UPDATE ON moa.knowledge_connection_disconnect_progress
FOR EACH ROW EXECUTE FUNCTION moa.enforce_knowledge_connection_disconnect_transition();

ALTER TABLE moa.knowledge_connection_disconnect_progress ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.knowledge_connection_disconnect_progress FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.knowledge_connection_disconnect_progress
FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

GRANT SELECT, INSERT, UPDATE ON moa.knowledge_connection_disconnect_progress TO moa_app;

-- Purge the disconnect ledger immediately before its knowledge child, and the
-- managed-parent ledger immediately before the generic connector parent. The
-- remote offset avoids transient primary-key collisions while preserving the
-- complete existing dependency order.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= 25;
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 999
WHERE stage_order >= 1025;

INSERT INTO moa.tenant_purge_catalog
    (stage_order, stage_name, table_schema, table_name, scope_mode, action_mode)
VALUES
    (25, 'moa.knowledge_connection_disconnect_progress', 'moa',
     'knowledge_connection_disconnect_progress', 'tenant_id', 'delete');

UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= 30;
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 999
WHERE stage_order >= 1030;

INSERT INTO moa.tenant_purge_catalog
    (stage_order, stage_name, table_schema, table_name, scope_mode, action_mode)
SELECT
    parent.stage_order - 1,
    'moa.connector_managed_parent_claims',
    'moa',
    'connector_managed_parent_claims',
    'tenant_id',
    'delete'
FROM moa.tenant_purge_catalog AS parent
WHERE parent.stage_name = 'moa.connector_connections';

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 133-table tenant-offboarding residue surface. The two nullable-scope simulator certification authority tables are intentionally global and absent.';

CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.connector_managed_parent_claims
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.connector_managed_parent_claims
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.knowledge_connection_disconnect_progress
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.knowledge_connection_disconnect_progress
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');

-- V51 pins the exact predecessor catalog cardinality. Replace only the two
-- expected literals so any drift in the bounded purge proof fails closed.
DO $knowledge_connection_parent_purge$
DECLARE
    predecessor TEXT;
    replacement TEXT;
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 131%'
       OR predecessor NOT LIKE '%exactly 131 tables%'
    THEN
        RAISE EXCEPTION 'unexpected V51 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, 'catalog_count <> 131', 'catalog_count <> 133');
    replacement := replace(replacement, 'exactly 131 tables', 'exactly 133 tables');
    EXECUTE replacement;
END
$knowledge_connection_parent_purge$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;
