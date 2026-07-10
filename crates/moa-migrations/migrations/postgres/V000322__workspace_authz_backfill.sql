-- Backfill workspace-to-tenant authorization edges for the hard-break
-- workspace-admin OpenFGA model. The deterministic local/default workspace id
-- matches `moa_core::WORKSPACE_ID`.

DO $$
DECLARE
    default_workspace_id CONSTANT UUID := '00000000-0000-0000-0000-000000000001'::UUID;
BEGIN
    CREATE TEMP TABLE workspace_authz_backfill_tenants (
        tenant_id UUID PRIMARY KEY
    ) ON COMMIT DROP;

    INSERT INTO workspace_authz_backfill_tenants (tenant_id)
    SELECT DISTINCT tenant_id
    FROM api_keys
    WHERE tenant_id IS NOT NULL
    ON CONFLICT DO NOTHING;

    INSERT INTO workspace_authz_backfill_tenants (tenant_id)
    SELECT DISTINCT tenant_id
    FROM users
    WHERE tenant_id IS NOT NULL
    ON CONFLICT DO NOTHING;

    INSERT INTO workspace_authz_backfill_tenants (tenant_id)
    SELECT DISTINCT tenant_id
    FROM sessions
    WHERE tenant_id IS NOT NULL
    ON CONFLICT DO NOTHING;

    INSERT INTO workspace_authz_backfill_tenants (tenant_id)
    SELECT DISTINCT tenant_id
    FROM moa.knowledge_connections
    WHERE tenant_id IS NOT NULL
    ON CONFLICT DO NOTHING;

    IF to_regclass('moa.artifact') IS NOT NULL
       AND EXISTS (
           SELECT 1
           FROM information_schema.columns
           WHERE table_schema = 'moa'
             AND table_name = 'artifact'
             AND column_name = 'tenant_id'
       )
    THEN
        EXECUTE 'INSERT INTO workspace_authz_backfill_tenants (tenant_id)
                 SELECT DISTINCT tenant_id
                 FROM moa.artifact
                 WHERE tenant_id IS NOT NULL
                 ON CONFLICT DO NOTHING';
    END IF;

    DELETE FROM authz_outbox
    WHERE status IN ('pending', 'dead_letter')
      AND tuple_object LIKE 'tenant:%'
      AND tuple_relation IN ('member', 'scim_admin');

    -- Desired state = write the workspace->tenant edge. Mirror the outbox
    -- upsert semantics: reactivate the identity only if it currently carries a
    -- different op or was dead-lettered, so an already-pending/succeeded write
    -- is left untouched.
    INSERT INTO authz_outbox
        (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id,
         generation, status, attempts, next_attempt_at)
    SELECT
        'write',
        'workspace:' || default_workspace_id,
        'workspace',
        'tenant:' || tenant_id,
        4,
        tenant_id,
        1, 'pending', 0, NOW()
    FROM workspace_authz_backfill_tenants
    ON CONFLICT (tuple_user, tuple_relation, tuple_object, model_version) DO UPDATE
    SET op = EXCLUDED.op,
        generation = authz_outbox.generation + 1,
        status = 'pending',
        attempts = 0,
        last_error = NULL,
        lease_token = NULL,
        lease_expires_at = NULL,
        next_attempt_at = NOW(),
        updated_at = NOW()
    WHERE authz_outbox.op IS DISTINCT FROM EXCLUDED.op
       OR authz_outbox.status = 'dead_letter';
END $$;
