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

    INSERT INTO authz_outbox
        (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
    SELECT
        format(
            'write-tenant:%s-workspace-workspace:%s-v3',
            tenant_id,
            default_workspace_id
        ),
        'write',
        'workspace:' || default_workspace_id,
        'workspace',
        'tenant:' || tenant_id,
        3,
        tenant_id
    FROM workspace_authz_backfill_tenants
    ON CONFLICT (idempotency_key) DO NOTHING;
END $$;
