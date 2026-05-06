# 04 — Memory Architecture

_Graph memory, privacy filtering, sidecar indexes, pgvector semantic retrieval, and consolidation._

## Principles

1. Graph memory is canonical; derived indexes are maintained from graph writes.
2. Every memory item has an explicit scope: tenant, workspace, and optional workspace-bound user.
3. Writes are attributable, bitemporal, privacy-classified, and auditable.
4. Retrieval combines graph structure, sidecar filters, keyword search, and vector similarity.
5. Memory is part of the learning pipeline, not a separate cache.

The graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`) is the only memory subsystem. The legacy file-wiki crate `moa-memory` was removed in C06; see `docs/migrations/moa-memory-inventory.md` for the per-consumer migration record.

## Scopes

| Scope | Contents |
|---|---|
| Global | Organization-wide conventions, shared concepts, promoted facts |
| Workspace | Project architecture, conventions, decisions, sources, and reusable lessons |
| User | Workspace-bound preferences, habits, and corrections for one user |

Graph writes set scope context before touching Postgres. Row-level security, changelog rows, sidecar projections, and vector records all use the same scope boundary.

## Graph Model

Memory is stored as typed graph nodes:

- `Entity`
- `Concept`
- `Decision`
- `Incident`
- `Lesson`
- `Fact`
- `Source`

Edges represent relationships, evidence, provenance, supersession, contradiction, and source attribution. Bitemporal validity lets new facts supersede older facts without erasing history.

## Sidecar And Vector Indexes

`moa-memory-graph` owns the graph tables and SQL sidecars used by operational reads. The sidecars provide fast filters for labels, names, scopes, timestamps, and active validity windows.

`moa-memory-vector` owns vector storage for semantic retrieval. Embeddings are written for graph nodes that should participate in retrieval, and hybrid retrieval fuses graph/sidecar candidates with vector hits.

Indexes are write-incremental. There is no user-facing rebuild-index command for the removed wiki store.

## Ingestion

Memory enters the graph through two routes:

- **Slow path**: `moa-memory-ingest` processes longer source text or turns through the ingestion VO. It chunks content, extracts facts/entities, classifies privacy, writes nodes and edges, embeds retrievable records, and records contradictions.
- **Fast path**: short observations use remember/forget/supersede APIs for direct graph writes with the same scope and privacy controls.

PII classification runs before durable memory writes. Sensitive text is either filtered, redacted, or tagged according to the privacy class and policy.

## Context Pipeline Integration

The memory processor runs after query rewriting and before history compilation. It uses the rewritten query when available, otherwise it extracts keywords from the latest user message.

It inserts ranked graph hits with labels, names, properties, provenance, and concise snippets. Memory content is inserted near the active turn so static prompt prefix caching remains stable.

## Consolidation

Workspace consolidation is a scheduled maintenance pass. In cloud mode it is the `Consolidate` Restate workflow. Locally it runs through the local maintenance path.

Consolidation can:

- resolve contradictions with superseding edges
- prune or expire stale facts
- merge duplicate nodes
- refresh sidecar and vector projections
- record memory learning entries for audit

Successful consolidation appends a `memory_updated` entry to `learning_log`.

## Learning Relationship

Memory is one output of the broader learning loop:

```text
Task segments
  -> resolution scores
  -> learning_log
  -> skill ranking, intent discovery, graph memory consolidation
```

Graph memory describes current knowledge; `learning_log` explains how and when a learned update entered the system.
