# Findings

## Architecture

- `docs/01-architecture-overview.md` says graph memory is canonical, with sidecar
  and vector indexes maintained by graph writes.
- `docs/04-memory-architecture.md` says pgvector is the default backend and
  Turbopuffer is a vector backend only; graph storage stays in relational
  Postgres nodes and edges.

## Current Code

- `VectorStore::sync_post_commit` is currently a graph-write lifecycle method on
  the generic vector trait.
- `PostgresGraphStore` stores only `Option<Arc<dyn VectorStore>>`, so graph
  writes call the hook through the vector trait after each transaction commit.
- `VectorStoreFactory::transactional_graph_store` correctly writes pgvector and
  queues outbox rows for external backends, but retrieval builders can misuse it
  and thereby read pgvector instead of configured Turbopuffer.
- `drain_external_sync` currently claims a batch but processes rows one at a
  time, reloading backend state and issuing one external request per row.
- Slow-path ingest calls `configured_for_scope` inside per-fact/per-decision
  loops. Most turns use one or two scopes, so a turn-local scoped cache is enough.
- `GraphMemoryRetriever` already caches non-contact scoped runtimes. The missing
  piece is that runtime construction must use the configured vector backend.
- Fast-path forget builds a full `FastPathCtx`, including read-side configured
  vector selection, even though forget only uses graph invalidation.

## Test Strategy

- Use DB integration tests for outbox batching and graph post-commit timing.
- Use unit tests around configurable factories where DB state is not required.
- Use existing fast-path and pipeline tests for cache/lazy-construction behavior
  when possible instead of adding broad new harnesses.

