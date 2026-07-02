# Vector Backend Followups

## Goal

Implement the long-term followups from the vector-store factory/outbox work:
batch external vector sync, add an explicit background drain path, cache configured
vector stores for ingest/retrieval scopes, and move graph post-commit sync out of
the generic `VectorStore` trait.

## Done When

- [ ] External vector sync drains group claimed rows by storage partition and batch
  upsert/delete calls where failure isolation still leaves retryable outbox rows.
- [ ] Graph writes attach an explicit post-commit vector sync hook instead of
  requiring every `VectorStore` implementation to expose a graph lifecycle method.
- [ ] Retrieval paths use the configured vector backend for reads and construct it
  once per scoped runtime instead of using transactional pgvector wrappers.
- [ ] Slow-path ingest reuses configured vector stores per `(scope, role)` within a
  turn instead of selecting the backend per fact.
- [ ] Fast-path forget does not eagerly construct read-side vector dependencies it
  never uses.
- [ ] Focused integration/unit tests pin batching, partition isolation, hook timing,
  configured retrieval selection, and lazy fast-path vector construction.
- [ ] Verification passes: `cargo fmt --all`, focused tests, focused clippy,
  `cargo build --workspace`, `git diff --check`, and `graphify update .`.

## Phases

1. [complete] Map current vector sync, graph write, ingest, and retrieval seams.
2. [complete] Refactor graph post-commit sync into an explicit hook attached to
   `PostgresGraphStore`.
3. [complete] Batch vector outbox drain by partition and operation, including batch
   status updates.
4. [complete] Add a background/maintenance drain entrypoint for committed backlog.
5. [complete] Cache configured vector stores in slow ingest and retrieval runtime
   construction; avoid eager vector selection for fast forget.
6. [complete] Strengthen tests and run focused verification.
7. [complete] Refresh graphify and final hygiene.

## Design Decisions

- Postgres graph memory remains canonical. External vector backends are derived
  projections synced after commit from durable outbox rows.
- Transactional graph writes continue to use pgvector as the Postgres source.
- Retrieval reads should use the configured vector backend, not the transactional
  graph-write wrapper.
- Post-commit sync is a graph-store attachment concern, not a method on all
  vector stores.
- Background backlog drains are explicit API/maintenance calls; graph writes may
  perform a small partition-scoped best-effort drain only for their own partition.
