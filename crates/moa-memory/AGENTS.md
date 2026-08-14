# Memory Instructions

Read `docs/04-memory-architecture.md`, `docs/07-context-pipeline.md`, and
`docs/15-architecture-policy.md`. Graph storage is canonical; vector and
sidecar indexes are derived. Preserve tenant/storage-partition scope, RLS through
`ScopedConn`, sensitivity handling, ingestion provenance, and graph/vector
write ordering. Do not move memory-owned types into `moa-core`.

Use `fast-pr` for pure logic and `db-memory` for scoped storage behavior. Live
retrieval/provider evaluations remain ignored until their named authorization,
credential, and budget gates are explicitly granted.
