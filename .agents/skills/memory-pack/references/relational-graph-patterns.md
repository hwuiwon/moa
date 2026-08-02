# Relational Graph Patterns

These rules apply to MOA's Postgres-backed graph memory. Graph state lives in
ordinary relational tables such as `moa.node_index`, `moa.edge_index`, graph
changelog rows, and vector rows. Do not add external graph-database assumptions
to new memory-pack work.

## Query Safety

- Use SQL parameters or `sqlx::QueryBuilder` binds for dynamic values.
- Never format user text, labels, node UIDs, edge labels, storage partitions, or
  scope values directly into SQL strings.
- Keep `storage_partition_id`, generated `scope`, and `user_id` predicates
  explicit on graph reads and writes. RLS is a safety net; query shape should
  still make the intended boundary obvious.
- Use typed projection structs when reading graph rows. Avoid passing raw
  `serde_json::Value` blobs through the retrieval path unless the owning type is
  intentionally dynamic.

## Traversals

- Prefer direct indexed joins over recursive SQL when one-hop or bounded-hop
  logic is enough.
- Use recursive SQL only for genuinely graph-shaped traversal, and carry depth,
  direction, allowed edge labels, and visited-node protection explicitly.
- Keep traversal limits small and request-scoped. Broad graph expansion must be
  justified by retrieval-quality measurements.

## Where To Look

- `crates/moa-memory/graph/src/read.rs` for current relational graph read
  helpers.
- `crates/moa-memory/graph/src/write.rs` for graph write and changelog
  patterns.
- `crates/moa-migrations/migrations/postgres/V000002__session_baseline.sql`
  for `moa.node_index`, `moa.edge_index`, embeddings, RLS helpers, and indexes.
- `docs/04-memory-architecture.md` for the graph-memory model.
