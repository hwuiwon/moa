# 04 — Memory Architecture

_Graph memory, privacy filtering, sidecar indexes, pgvector semantic retrieval, and consolidation._

## Principles

1. Graph memory is canonical; derived indexes are maintained from graph writes.
2. Every memory item has an explicit scope: tenant, workspace, and optional workspace-bound user.
3. Writes are attributable, bitemporal, privacy-classified, and auditable.
4. Retrieval combines graph structure, sidecar filters, keyword search, and vector similarity.
5. Memory is part of the learning pipeline, not a separate cache.

The graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`) is the only memory subsystem. See `crates/moa-memory/README.md` for crate-level details and `docs/15-architecture-policy.md` for ownership rules.

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

`moa-memory-vector` owns vector storage for semantic retrieval. Embeddings are written for graph nodes that should participate in retrieval, and hybrid retrieval fuses graph/sidecar candidates with vector hits. The default backend is pgvector; large or isolation-sensitive workspaces can opt into Turbopuffer namespaces through `workspace_state.vector_backend`.

Embedder selection is per workspace. `cohere-embed-v4` and `gemini-embedding-2` use incompatible vector spaces, so switching a workspace requires re-embedding its graph nodes before retrieval can safely use the new model. Gemini Embedding 2 is exposed as a text-only `Embedder` today; its API supports multimodal inputs, but MOA needs a separate multimodal chunker and embedder trait before image, audio, video, or PDF chunks are indexed.

Gemini Embedding 2 does not use a `task_type` request field. MOA encodes asymmetric retrieval through role-specific prompt prefixes inside the embedder: ingestion-side embedders use the document prefix and retrieval-side embedders use a search-query prefix.

Indexes are write-incremental. There is no user-facing rebuild-index command for graph memory.

## Ingestion

Memory enters the graph through two routes:

- **Slow path**: `moa-memory-ingest` processes longer source text or turns through the ingestion VO. It chunks content, extracts facts/entities, classifies privacy, writes nodes and edges, embeds retrievable records, and records contradictions.
- **Fast path**: short observations use remember/forget/supersede APIs for direct graph writes with the same scope and privacy controls.

Slow-path fact extraction is behind the `FactExtractor` seam. The heuristic extractor remains the default and journal-safe fallback. Environments can opt into the Cohere-backed `LlmFactExtractor` with `memory.extraction.enabled` plus the configured API-key env var; eval replay uses recorded extraction fixtures so the natural transcript lane stays hermetic after live recording.

PII classification runs before durable memory writes. Sensitive text is either filtered, redacted, or tagged according to the privacy class and policy.

## Context Pipeline Integration

The memory processor runs after query rewriting and before history compilation. It uses the rewritten query when available, otherwise it extracts keywords from the latest user message.

It inserts ranked graph hits with labels, names, properties, provenance, and concise snippets. Memory content is inserted near the active turn so static prompt prefix caching remains stable.

## Consolidation

Workspace consolidation is a scheduled maintenance pass. In cloud mode it is the `Consolidate` Restate workflow. Locally and in eval it runs through the shared `moa-memory-lifecycle` crate. The workflow is a thin durable wrapper; the memory logic does not depend on Restate, so hermetic eval runs and scheduled maintenance call the same code.

Consolidation v1 runs four deterministic operations:

- **Exact duplicate merge** groups active `Fact` nodes by `(workspace_id, user_id, scope, fact_hash)`. The canonical is the earliest `valid_from` row with UID as the tiebreak. Other active rows are closed with a `SUPERSEDES` edge in the same direction as normal graph supersession: replacement/canonical `-> SUPERSEDES -> old`.
- **Anchored confidence decay** lowers confidence for idle facts. On first decay the current confidence is copied to `properties.base_confidence`; future runs recompute from that base instead of multiplying against the current value. This makes rerunning at the same `now` idempotent. Decay floors at the configured minimum and never deletes or invalidates a fact.
- **Contradiction sweep** groups active facts by `(workspace_id, user_id, scope, subject, predicate)`. If a group contains multiple objects, the newest `valid_from` row wins with UID as the deterministic tiebreak, and older rows are superseded. No LLM judge runs in v1.
- **Entity backfill** embeds active `Entity` nodes that lack vector rows when an embedder is available, and promotes edge-level `alias_mention` values into `properties.aliases` through the graph property-update operation.

The v1 pass deliberately does not do semantic near-duplicate merging, digest building, episode building, scope-drift repair, or destructive expiry. `at_floor` is reported for future policy design, but floor-bound facts remain active unless another write supersedes them.

Successful consolidation appends a `memory_updated` entry to `learning_log`.

## Learning Relationship

Memory is one output of the broader learning loop:

```text
Task segments
  -> resolution scores
  -> learning_log
  -> skill ranking and graph memory consolidation
```

Graph memory describes current knowledge; `learning_log` explains how and when a learned update entered the system.
