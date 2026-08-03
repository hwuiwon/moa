-- Same-tenant desired-state registry for direct connector `Use` grants.
--
-- OpenFGA remains the authorization decision point. This table is the durable,
-- tenant-scoped registry that lets connection deletion enqueue exact inverse
-- tuples without enumerating a remote authorization store. Physical deletion of
-- a connection or subject is deliberately restricted while a grant exists; the
-- owner must first enqueue the inverse tuple and delete the registry row in the
-- same transaction.

-- Composite keys make the polymorphic subject references tenant-exact. The
-- primary keys already make each identity globally unique, but PostgreSQL needs
-- an explicitly matching unique key for the composite foreign keys below.
CREATE UNIQUE INDEX users_id_tenant_key ON public.users (id, tenant_id);
CREATE UNIQUE INDEX agents_id_tenant_key ON public.agents (id, tenant_id);
CREATE UNIQUE INDEX contacts_id_tenant_key ON public.contacts (id, tenant_id);

CREATE TABLE moa.connector_connection_use_grants (
    tenant_id UUID NOT NULL,
    connection_uid UUID NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    operator_subject_id UUID GENERATED ALWAYS AS (
        CASE WHEN subject_kind = 'operator' THEN subject_id END
    ) STORED,
    agent_subject_id UUID GENERATED ALWAYS AS (
        CASE WHEN subject_kind = 'agent' THEN subject_id END
    ) STORED,
    contact_subject_id UUID GENERATED ALWAYS AS (
        CASE WHEN subject_kind = 'contact' THEN subject_id END
    ) STORED,
    CONSTRAINT connector_connection_use_grants_subject_kind_valid
        CHECK (subject_kind IN ('operator', 'agent', 'contact')),
    CONSTRAINT connector_connection_use_grants_desired_state_key
        PRIMARY KEY (tenant_id, connection_uid, subject_kind, subject_id),
    CONSTRAINT connector_connection_use_grants_connection_fk
        FOREIGN KEY (connection_uid, tenant_id)
        REFERENCES moa.connector_connections (connection_uid, tenant_id),
    CONSTRAINT connector_connection_use_grants_operator_fk
        FOREIGN KEY (operator_subject_id, tenant_id)
        REFERENCES public.users (id, tenant_id),
    CONSTRAINT connector_connection_use_grants_agent_fk
        FOREIGN KEY (agent_subject_id, tenant_id)
        REFERENCES public.agents (id, tenant_id),
    CONSTRAINT connector_connection_use_grants_contact_fk
        FOREIGN KEY (contact_subject_id, tenant_id)
        REFERENCES public.contacts (id, tenant_id)
);

ALTER TABLE moa.connector_connection_use_grants ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.connector_connection_use_grants FORCE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON moa.connector_connection_use_grants
FOR ALL TO moa_app
    USING (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID)
    WITH CHECK (tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID);

-- A grant has no mutable state: grant inserts one desired relationship and
-- revoke deletes it after enqueueing the matching OpenFGA inverse.
GRANT SELECT, INSERT, DELETE ON moa.connector_connection_use_grants TO moa_app;

-- Subject validation crosses the connector registry boundary into identity
-- tables. Keep those tables unavailable to the runtime role and expose only
-- tenant-exact existence/eligibility predicates. The explicit current-tenant
-- comparison prevents a caller from using the SECURITY DEFINER functions to
-- probe another tenant by supplying an arbitrary tenant UUID.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_roles
        WHERE rolname = 'moa_connector_subject_validator'
    ) THEN
        CREATE ROLE moa_connector_subject_validator
            NOLOGIN NOINHERIT NOBYPASSRLS;
    ELSE
        ALTER ROLE moa_connector_subject_validator
            NOLOGIN NOINHERIT NOBYPASSRLS;
    END IF;
END;
$$;

GRANT USAGE ON SCHEMA public, moa TO moa_connector_subject_validator;
GRANT SELECT (id, tenant_id, active) ON public.users
    TO moa_connector_subject_validator;
GRANT SELECT (id, tenant_id, status) ON public.agents
    TO moa_connector_subject_validator;
GRANT SELECT (id, tenant_id, state) ON public.contacts
    TO moa_connector_subject_validator;

-- Contacts already have forced tenant RLS. Give the dedicated function owner
-- the same exact tenant visibility; users and agents are bounded by the
-- function predicates and cannot be queried directly by `moa_app`.
CREATE POLICY connector_subject_validator_read ON public.contacts
    FOR SELECT TO moa_connector_subject_validator
    USING (
        tenant_id = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
    );

CREATE FUNCTION moa.connector_use_subject_exists(
    requested_tenant_id UUID,
    requested_subject_kind TEXT,
    requested_subject_id UUID
) RETURNS BOOLEAN
LANGUAGE SQL
STABLE
SECURITY DEFINER
SET search_path = pg_catalog
AS $$
    SELECT requested_tenant_id
               = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
       AND CASE requested_subject_kind
               WHEN 'operator' THEN EXISTS (
                   SELECT 1
                   FROM public.users
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
               )
               WHEN 'agent' THEN EXISTS (
                   SELECT 1
                   FROM public.agents
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
               )
               WHEN 'contact' THEN EXISTS (
                   SELECT 1
                   FROM public.contacts
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
               )
               ELSE FALSE
           END;
$$;

CREATE FUNCTION moa.connector_use_subject_is_eligible(
    requested_tenant_id UUID,
    requested_subject_kind TEXT,
    requested_subject_id UUID
) RETURNS BOOLEAN
LANGUAGE SQL
STABLE
SECURITY DEFINER
SET search_path = pg_catalog
AS $$
    SELECT requested_tenant_id
               = NULLIF(current_setting('moa.tenant_id', TRUE), '')::UUID
       AND CASE requested_subject_kind
               WHEN 'operator' THEN EXISTS (
                   SELECT 1
                   FROM public.users
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
                     AND active
               )
               WHEN 'agent' THEN EXISTS (
                   SELECT 1
                   FROM public.agents
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
                     AND status = 'active'
               )
               WHEN 'contact' THEN EXISTS (
                   SELECT 1
                   FROM public.contacts
                   WHERE id = requested_subject_id
                     AND tenant_id = requested_tenant_id
                     AND state <> 'merged'
               )
               ELSE FALSE
           END;
$$;

ALTER FUNCTION moa.connector_use_subject_exists(UUID, TEXT, UUID)
    OWNER TO moa_connector_subject_validator;
ALTER FUNCTION moa.connector_use_subject_is_eligible(UUID, TEXT, UUID)
    OWNER TO moa_connector_subject_validator;
REVOKE ALL ON FUNCTION moa.connector_use_subject_exists(UUID, TEXT, UUID)
    FROM PUBLIC;
REVOKE ALL ON FUNCTION moa.connector_use_subject_is_eligible(UUID, TEXT, UUID)
    FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.connector_use_subject_exists(UUID, TEXT, UUID)
    TO moa_app;
GRANT EXECUTE ON FUNCTION moa.connector_use_subject_is_eligible(UUID, TEXT, UUID)
    TO moa_app;

-- Insert the registry after invocation/binding children and immediately before
-- the connection parent. The remote key range avoids transient stage collisions.
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order + 1000
WHERE stage_order >= 28;
UPDATE moa.tenant_purge_catalog
SET stage_order = stage_order - 999
WHERE stage_order >= 1028;

INSERT INTO moa.tenant_purge_catalog
    (stage_order, stage_name, table_schema, table_name, scope_mode, action_mode)
VALUES
    (28, 'moa.connector_connection_use_grants', 'moa',
     'connector_connection_use_grants', 'tenant_id', 'delete');

COMMENT ON TABLE moa.tenant_purge_catalog IS
    'Closed 131-table tenant-offboarding residue surface. The two nullable-scope simulator certification authority tables are intentionally global and absent.';

CREATE TRIGGER moa_tenant_purge_fence_insert
AFTER INSERT ON moa.connector_connection_use_grants
REFERENCING NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');
CREATE TRIGGER moa_tenant_purge_fence_update
AFTER UPDATE ON moa.connector_connection_use_grants
REFERENCING OLD TABLE AS tenant_purge_old_rows NEW TABLE AS tenant_purge_new_rows
FOR EACH STATEMENT EXECUTE FUNCTION moa.guard_tenant_write_statement('tenant_id');

-- V50 pins the exact predecessor catalog cardinality. Replace only the two
-- expected literals so a drifted function definition fails closed.
DO $connector_use_grant_purge$
DECLARE
    predecessor TEXT;
    replacement TEXT;
BEGIN
    SELECT pg_get_functiondef('moa.run_tenant_purge_batch(uuid,text)'::REGPROCEDURE)
    INTO predecessor;
    IF predecessor NOT LIKE '%catalog_count <> 130%'
       OR predecessor NOT LIKE '%exactly 130 tables%'
    THEN
        RAISE EXCEPTION 'unexpected V50 tenant purge function definition'
            USING ERRCODE = '55000';
    END IF;
    replacement := replace(predecessor, 'catalog_count <> 130', 'catalog_count <> 131');
    replacement := replace(replacement, 'exactly 130 tables', 'exactly 131 tables');
    EXECUTE replacement;
END
$connector_use_grant_purge$;

ALTER FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) OWNER TO moa_owner;
REVOKE ALL ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION moa.run_tenant_purge_batch(UUID, TEXT)
    TO moa_app, moa_promoter;

-- BEGIN STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT
ALTER TABLE tenant_credential_operations
    ADD COLUMN IF NOT EXISTS expected_prior_credential_uid UUID;
ALTER TABLE tenant_credential_operations
    ALTER COLUMN slot_name DROP NOT NULL;
ALTER TABLE tenant_credential_operations
    DROP CONSTRAINT IF EXISTS tenant_credential_operations_operation_valid;
ALTER TABLE tenant_credential_operations
    ADD CONSTRAINT tenant_credential_operations_operation_valid
        CHECK (operation IN (
            'create', 'stage', 'activate', 'rollback_activation', 'resolve',
            'rotate', 'revoke', 'delete'
        ));
ALTER TABLE tenant_credential_operations
    ADD CONSTRAINT tenant_credential_operations_selector_valid
        CHECK (
            slot_name IS NOT NULL
            OR (
                operation = 'revoke'
                AND credential_uid IS NULL
                AND connection_uid IS NOT NULL
                AND kind IS NULL
                AND version IS NULL
            )
        );
-- END STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT
