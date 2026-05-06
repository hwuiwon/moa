# moa-memory

Graph-primary memory subsystem for MOA. These four subcrates form one logical
unit while keeping graph storage, embeddings, privacy filtering, and ingestion
separate.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `graph/` | `moa-memory-graph` | `GraphStore`, AGE-backed graph storage, sidecar projection, bi-temporal writes, and node/edge label registries. |
| `vector/` | `moa-memory-vector` | Embedder abstraction, provider bindings, pgvector storage, Turbopuffer opt-in storage, and embedding queue support. |
| `pii/` | `moa-memory-pii` | PII classification and redaction before durable memory writes. |
| `ingest/` | `moa-memory-ingest` | Restate `IngestionVO` slow path and inline fast memory writes. |

## Public Surface

Consumers depend on the package names, not these folder names:
`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, and
`moa-memory-ingest`.

The retriever lives in `moa-brain` because it composes graph, vector, and query
planning concerns.

For type ownership across the memory crates, see
`docs/architecture/type-placement.md`.
