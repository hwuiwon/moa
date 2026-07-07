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
        'write-workspace:00000000-0000-0000-0000-000000000001-admin-operator:<workspace-admin-operator-id>-v4',
        'write',
        'operator:<workspace-admin-operator-id>',
        'admin',
        'workspace:00000000-0000-0000-0000-000000000001',
        3,
        NULL
    )
ON CONFLICT (idempotency_key) DO NOTHING;
```

After a workspace admin exists, grant additional users workspace-admin access:

```sh
curl -X POST "$MOA_EDGE_URL/v1/authz/tuple-write" \
  -H "Authorization: Bearer <workspace-admin-key>" \
  -H "Content-Type: application/json" \
  --data '{"user":"operator:<workspace-admin-operator-id>","relation":"admin","object":"workspace:00000000-0000-0000-0000-000000000001"}'
```

Attach a tenant to the workspace:

```sh
curl -X POST "$MOA_EDGE_URL/v1/authz/tuple-write" \
  -H "Authorization: Bearer <workspace-admin-key>" \
  -H "Content-Type: application/json" \
  --data '{"user":"workspace:00000000-0000-0000-0000-000000000001","relation":"workspace","object":"tenant:<tenant-id>","tenant_id":"<tenant-id>"}'
```

Normal tenant provisioning and tenant-admin setup paths enqueue the tenant
attachment automatically. Existing tenants are backfilled by the workspace authz
migration using the canonical workspace id. Every tenant should have exactly one
current tenant-to-workspace tuple.
