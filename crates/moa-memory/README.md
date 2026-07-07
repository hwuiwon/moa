# moa-memory

Graph-primary memory subsystem for MOA. These six subcrates form one logical
unit while keeping graph storage, embeddings, privacy filtering, ingestion,
lifecycle maintenance, and shared types separate.

## Subcrates

| Path | Crate name | Responsibility |
| --- | --- | --- |
| `graph/` | `moa-memory-graph` | `GraphStore`, relational graph storage, sidecar projection, bi-temporal writes, and node/edge label registries. |
| `vector/` | `moa-memory-vector` | Vector store abstractions, pgvector transactional storage, Turbopuffer read-side projection, backend promotion, and vector-sync outbox support. |
| `pii/` | `moa-memory-pii` | PII classification and redaction before durable memory writes. |
| `ingest/` | `moa-memory-ingest` | Restate `IngestionVO` slow path and inline fast memory writes. |
| `lifecycle/` | `moa-memory-lifecycle` | Memory consolidation, quality scoring, and digest generation. |
| `types/` | `moa-memory-types` | Shared memory domain types used across the memory subcrates. |

## Public Surface

Consumers depend on the package names, not these folder names:
`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`,
`moa-memory-ingest`, `moa-memory-lifecycle`, and `moa-memory-types`.

The retriever lives in `moa-brain` because it composes graph, vector, and query
planning concerns.

For type ownership across the memory crates, see
`docs/15-architecture-policy.md`.
