# moa-memory-graph

Relational graph-memory store for MOA. Implements `PostgresGraphStore` over
plain Postgres tables with bi-temporal validity, an SQL sidecar projection
(`moa.node_index`) for lexical lookup, and an append-only changelog outbox.

## Structure

- `store` — relational Postgres-backed `GraphStore` implementation.
- `read` — read-side implementation for the relational graph store, including
  seed-expansion walks (`GraphExpansionHit`, `GraphWalkScoring`).
- `write` — atomic graph write protocol for relational rows, vectors, and
  changelog records.
- `node` — SQL projection helpers and write intents for graph nodes.
- `edge` — edge labels and write intents for graph-memory relationships.
- `lexical` — lexical lookup over the `moa.node_index` sidecar.
- `validity` — shared bitemporal validity predicates for graph-memory reads.
- `changelog` — append-only graph changelog outbox writer.
- `error` — error type for graph-memory operations.

## Place In The Memory Family

Depends on `moa-memory-vector` so graph writes can commit embeddings in the
same transaction. `moa-memory-pii`, `moa-memory-ingest`,
`moa-memory-lifecycle`, and `moa-knowledge` all build on this crate.
