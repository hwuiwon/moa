# Workspace Admin Bootstrap

Workspace admins are deployment-level super-admin principals represented in
OpenFGA as `workspace#admin`. Tenant access is inherited through the tuple that
links each tenant to the workspace.

MOA has exactly one workspace. Its canonical OpenFGA object id is
`workspace:00000000-0000-0000-0000-000000000001`.

Seed the first workspace admin through controlled ops by inserting the tuple
into the authz outbox or by running an equivalent deployment migration:

```sql
INSERT INTO authz_outbox
    (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
VALUES
    (
        'write-workspace-00000000-0000-0000-0000-000000000001-admin-operator-<workspace-admin-operator-id>-v4',
        'write',
        'operator:<workspace-admin-operator-id>',
        'admin',
        'workspace:00000000-0000-0000-0000-000000000001',
        4,
        NULL
    )
ON CONFLICT (idempotency_key) DO NOTHING;
```

After a workspace admin exists, grant additional users workspace-admin access
through the same controlled ops channel:

```sql
INSERT INTO authz_outbox
    (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
VALUES
    (
        'write-workspace-00000000-0000-0000-0000-000000000001-admin-operator-<workspace-admin-operator-id>-v4',
        'write',
        'operator:<workspace-admin-operator-id>',
        'admin',
        'workspace:00000000-0000-0000-0000-000000000001',
        4,
        NULL
    )
ON CONFLICT (idempotency_key) DO NOTHING;
```

Attach a tenant to the workspace:

```sql
INSERT INTO authz_outbox
    (idempotency_key, op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id)
VALUES
    (
        'write-tenant:<tenant-id>-workspace-workspace:00000000-0000-0000-0000-000000000001-v4',
        'write',
        'workspace:00000000-0000-0000-0000-000000000001',
        'workspace',
        'tenant:<tenant-id>',
        4,
        '<tenant-id>'::UUID
    )
ON CONFLICT (idempotency_key) DO NOTHING;
```

Normal tenant provisioning and tenant-admin setup paths enqueue the tenant
attachment automatically. Existing tenants are backfilled by the workspace authz
migration using the canonical workspace id and the current v4 authz model
version. Every tenant should have exactly one current tenant-to-workspace tuple.

The public authz administration route is intentionally limited to typed
API-key tenant role grants and revocations. Do not use public HTTP routes to
write workspace-admin or tenant-to-workspace tuples.

Tenant purge enqueues inverse tuple deletes for tenant, user, API-key, session,
contact-session, and agent tuples with the purged tenant id attached to every
outbox row. Agent tenant/operator tuples are collected from Postgres before row
deletion, and agent `can_act_as` delegation tuples are read from OpenFGA before
the purge transaction deletes agent rows.
