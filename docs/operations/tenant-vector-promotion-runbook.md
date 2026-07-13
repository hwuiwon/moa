# Tenant Vector Promotion Runbook

Promote a tenant from pgvector to Turbopuffer when the tenant grows past the
local HNSW operating range or needs namespace-level backend isolation.
Only vector records move during promotion. Graph nodes and edges remain in
Postgres relational storage (`moa.node_index` and `moa.edge_index`);
Turbopuffer is not graph storage.

## API Request

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/vector/promote" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "target_backend": "turbopuffer",
    "validate_percent": 5,
    "dual_read_hours": 24
  }'
```

Required environment:

- `MOA_TURBOPUFFER_API_KEY`
- `MOA_TURBOPUFFER_ENVIRONMENT` or `MOA_OBSERVABILITY_ENVIRONMENT`
- `MOA_TURBOPUFFER_BAA=true` for HIPAA or restricted-tier tenants

## What Happens

1. `moa.storage_partition_state.vector_backend_state` becomes `migrating` for the authenticated tenant.
2. All rows from `moa.embeddings` for the tenant are copied to the
   Turbopuffer namespace `moa-<env>-<tenant_id>` in batches of 256.
3. A deterministic sample is queried against both backends. Promotion requires
   at least `0.95` average top-K overlap.
4. The tenant vector backend flips to `vector_backend='turbopuffer'` and
   `vector_backend_state='dual_read'`.
5. During dual-read, the retriever queries both backends, returns Turbopuffer
   results, and falls back to pgvector on Turbopuffer failure. Backend agreement
   is gated before this step by the promotion sample in step 3 (≥ `0.95` average
   top-K overlap); a Turbopuffer failure that triggers the pgvector fallback logs
   a `Turbopuffer vector dual-read leg failed; using pgvector result` warning to
   watch during the window.

Every state flip increments the storage-local
`storage_partition_state.changelog_version`, which invalidates retrieval caches tied to
the tenant vector-state version.

## Rollback

Rollback is available during dual-read:

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/vector/rollback-promotion" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "action": "rollback"
  }'
```

This sets the tenant back to `vector_backend='pgvector'`,
`vector_backend_state='steady'`, clears `dual_read_until`, and bumps the
vector-state changelog version.

## Finalize

After the dual-read window is clean:

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/vector/finalize-promotion" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "action": "finalize"
  }'
```

This leaves `vector_backend='turbopuffer'`, sets
`vector_backend_state='steady'`, clears `dual_read_until`, and bumps the
vector-state changelog version. Dropping any pgvector partition or rows is an
operator-driven maintenance task outside this step.
