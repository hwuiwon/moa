# Workspace Promotion Runbook

Promote a workspace from pgvector to Turbopuffer when the workspace grows past
the local HNSW operating range or needs namespace-level backend isolation.

## API Request

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/promote-workspace" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "workspace_id": "<workspace-uuid>",
    "target_backend": "turbopuffer",
    "validate_percent": 5,
    "dual_read_hours": 24
  }'
```

Required environment:

- `TURBOPUFFER_API_KEY`
- `MOA_ENV` or `MOA_ENVIRONMENT`
- `TURBOPUFFER_BAA=1` for HIPAA or restricted-tier workspaces

## What Happens

1. `moa.workspace_state.vector_backend_state` becomes `migrating`.
2. All rows from `moa.embeddings` for the workspace are copied to the
   Turbopuffer namespace `moa-<env>-<workspace_id>` in batches of 256.
3. A deterministic sample is queried against both backends. Promotion requires
   at least `0.95` average top-K overlap.
4. The workspace flips to `vector_backend='turbopuffer'` and
   `vector_backend_state='dual_read'`.
5. During dual-read, the retriever queries both backends, records
   `moa_vector_dualread_overlap`, returns Turbopuffer results, and falls back to
   pgvector on Turbopuffer failure.

Every state flip increments `workspace_state.changelog_version`, which
invalidates retrieval caches tied to the workspace version.

## Rollback

Rollback is available during dual-read:

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/rollback-promotion" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "workspace_id": "<workspace-uuid>",
    "action": "rollback"
  }'
```

This sets the workspace back to `vector_backend='pgvector'`,
`vector_backend_state='steady'`, clears `dual_read_until`, and bumps the
workspace changelog version.

## Finalize

After the dual-read window is clean:

```sh
curl -sS "$MOA_EDGE_URL/v1/admin-maintenance/finalize-promotion" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "workspace_id": "<workspace-uuid>",
    "action": "finalize"
  }'
```

This leaves `vector_backend='turbopuffer'`, sets
`vector_backend_state='steady'`, clears `dual_read_until`, and bumps the
workspace changelog version. Dropping any pgvector partition or rows is an
operator-driven maintenance task outside this step.
