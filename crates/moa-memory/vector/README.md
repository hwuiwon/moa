# moa-memory-vector

Vector storage and embedding abstractions for graph memory. Provides the
transactional pgvector store, the Turbopuffer read-side projection, and the
machinery to promote a storage partition between backends. Embeddings are
fixed at `VECTOR_DIMENSION = 1024`.

## Structure

- `backend` — storage-partition vector-backend selection
  (`VectorStoreFactory`, `TransactionalGraphVectorBackend`).
- `pgvector_store` — pgvector-backed graph-memory vector store.
- `turbopuffer` — Turbopuffer-backed graph-memory vector store.
- `promotion` — vector partition backend promotion helpers with finalize and
  rollback paths.
- `sync` — durable vector-backend sync queue (outbox) for external vector
  projections.
- `embedding_row` — shared pgvector embedding row decoding helpers (crate
  private).

## Place In The Memory Family

The storage base of the chain: depends only on `moa-core`, `moa-config`, and
`moa-db` among workspace crates. `moa-memory-graph` builds on it so graph
writes and vector writes commit together, and the rest of the family
(`pii`, `ingest`, `lifecycle`) layers above that.
