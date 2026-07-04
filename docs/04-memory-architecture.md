# 04 — Memory Architecture

_Relational graph memory, privacy filtering, sidecar indexes, vector retrieval, and consolidation._

## Principles

1. Graph memory is canonical; derived indexes are maintained from graph writes.
2. Every memory item has an explicit tenant boundary and, for end-user memory,
   an explicit contact owner.
3. Writes are attributable, bitemporal, privacy-classified, and auditable.
4. Retrieval combines graph structure, sidecar filters, keyword search, and vector similarity.
5. Memory is part of the learning pipeline, not a separate cache.

The graph stack (`moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, `moa-memory-ingest`) is the only durable memory substrate. Tenant knowledge-base ingestion is a product layer in `moa-knowledge` that feeds this substrate; it is not a separate graph store and does not make connector providers part of `moa-memory-*`. See `crates/moa-memory/README.md` for crate-level details and `docs/15-architecture-policy.md` for ownership rules.

## Runtime Scopes

| Scope | Contents |
|---|---|
| Tenant knowledge | Tenant-owned synced source knowledge from linked systems, stored as graph-backed documents, chunks, facts, and entities |
| Tenant memory | Tenant-owned operational facts and admin/operator memory for that tenant |
| Contact memory | End-user preferences, facts, and corrections for one contact inside one tenant |

Graph writes set tenant context before touching Postgres. Contact-owned memory
also sets contact context. Row-level security, changelog rows, sidecar
projections, and vector records all use the same tenant boundary.

Agent-facing contacts are end users, not admin/operator users. Contact-bound
sessions store contact memory under the session tenant and contact. Anonymous,
unverified, and verified contacts write only their current contact memory. A
contact session does not inherit tenant admin/operator memory or any other
contact's memory. When graph memory is enabled, answer-time retrieval combines
tenant knowledge with admitted memory for the current contact by default. After
contact-point verification, session promotion moves the session to the canonical
verified contact; any historical merge or attribution repair must be an
explicit admin operation, not default retrieval.

## Graph Model

Memory is stored as typed graph nodes:

- `Entity`
- `Concept`
- `Decision`
- `Incident`
- `Lesson`
- `Fact`
- `Source`
- `Document`
- `Chunk`

Edges represent relationships, evidence, provenance, supersession, contradiction, and source attribution. Bitemporal validity lets new facts supersede older facts without erasing history.

Tenant knowledge uses `Source`, `Document`, `Chunk`, `Fact`, and `Entity`
labels. `moa-knowledge` derives graph deltas from parsed objects and calls
`moa-memory-graph` to write them. Required relationships are `HAS_DOCUMENT`
from source to document, `HAS_CHUNK` from document to chunk, `EVIDENCES` from
chunk to fact, `MENTIONS` from chunk to entity, and `DERIVED_FROM` from derived
contact groups or facts back to source evidence. Full chunk text is stored in
`moa.knowledge_chunks`; graph properties stay compact and citation-friendly.
Provider credentials, account tokens, raw credential material, and unbounded
raw source payloads are never graph properties.

## Sidecar And Vector Indexes

`moa-memory-graph` owns relational Postgres graph storage. Nodes live in
`moa.node_index`; edges live in `moa.edge_index`; SQL sidecars provide fast
filters for labels, names, scopes, timestamps, active validity windows, ranking,
and source hydration.

`moa-memory-vector` owns vector storage for semantic retrieval. Embeddings are
written for graph nodes that should participate in retrieval, and hybrid
retrieval fuses graph/sidecar candidates with vector hits. Local development
and tests default to pgvector. Cloud storage partitions that select the external
vector backend use Turbopuffer and must provide `MOA_TURBOPUFFER_API_KEY`;
missing credentials are treated as configuration errors rather than silently
falling back at retrieval time. Turbopuffer is a vector backend only: graph
storage, privacy state, historical/as-of reads, and the transactional write
source stay in relational Postgres nodes, edges, and pgvector rows. The
`vector_sync_outbox` projects committed pgvector writes into Turbopuffer after
the graph transaction commits.

`moa.node_index` also carries derived ranking metadata. `quality_score` is a
neutral-by-default `0.5` prior that FeatureV1 centers to zero contribution; a
score above or below that value can promote or demote otherwise similar facts
without changing graph truth. The value lives in the sidecar row rather than
node properties so candidate hydration does not parse dynamic properties per
query.

FeatureV1 token features (subject match, overlap) stem pure-alphabetic tokens
with Snowball so morphological variants match; identifier-like tokens with
digits are compared verbatim. First-person queries ("the way I prefer") double
the caller's contact-scope term, because such queries rarely share tokens with
the caller's stored facts. The lexical leg matches an OR `to_tsquery` over
extracted terms plus stems and ranks by `ts_rank`; the prior AND semantics
meant conversational queries almost never matched short fact names.

Known scaling caveat: graph expansion uses bounded recursive SQL over
`moa.node_index` and `moa.edge_index` under RLS, so cost grows with the caller's
tenant graph degree and traversal depth. The 250ms graph budget bounds latency
and silently trims as-of expansion on large shared databases.

Embedder selection is per tenant and uses a single `provider:model` selector,
for example `cohere:embed-v4.0` or `gemini:gemini-embedding-2`.
Those models use incompatible vector spaces, so switching a tenant requires
re-embedding its graph nodes before retrieval can safely use the new model.
Gemini Embedding 2 is exposed as a text-only `Embedder` today; its API supports
multimodal inputs, but MOA needs a separate multimodal chunker and embedder
trait before image, audio, video, or PDF chunks are indexed.

Gemini Embedding 2 does not use a `task_type` request field. MOA encodes asymmetric retrieval through role-specific prompt prefixes inside the embedder: ingestion-side embedders use the document prefix and retrieval-side embedders use a search-query prefix.

Indexes are write-incremental. There is no user-facing rebuild-index command for graph memory.

## Ingestion

Session and contact memory enter the graph through two routes:

- **Slow path**: `moa-memory-ingest` processes longer source text or turns through the ingestion VO. It chunks content, extracts facts/entities, classifies privacy, writes nodes and edges, embeds retrievable records, and records contradictions.
- **Fast path**: short observations use remember/forget/supersede APIs for direct graph writes with the same scope and privacy controls.

Tenant knowledge ingestion is a third route owned by `moa-knowledge`, not by
`Memory.ingest_documents`. It links external accounts through Nango or Merge,
triggers provider sync, lists changed records, fetches content, parses records
and files through `DocumentParser`, derives `KnowledgeBlock` and
`KnowledgeChunk` identities, computes graph deltas, and writes tenant-scoped
knowledge through `moa-memory-graph` and `moa-memory-vector`. The session
memory slow path may continue to accept explicit documents, but connector sync
must not synthesize session turns or overload `Memory.ingest_documents`.

`KnowledgeBlock` identity is `block_hash = blake3(normalized_text)`.
`KnowledgeChunk` identity is `chunk_hash = blake3(ordered block_hashes)`.
These identities let ingestion diff changed documents, reuse embeddings,
tombstone deleted source content, and keep citations stable across re-syncs.

Slow-path fact extraction is behind the `FactExtractor` seam. The heuristic extractor remains the default and journal-safe fallback. Environments can opt into provider-backed `ModelFactExtractor` with `memory.extraction.enabled`; model selection, credentials, and configured chat-model failover come from the shared `moa-providers` config path, while memory prompts, parsing, and prompt versions stay in `moa-memory-ingest`. Eval replay uses recorded extraction fixtures so the natural transcript lane stays hermetic after live recording.

PII classification runs before durable memory writes. Sensitive text is either filtered, redacted, or tagged according to the privacy class and policy.

## Context Pipeline Integration

The standing digest processor runs after query rewriting and before graph-memory
retrieval when `memory.digest.enabled` is true. Contact sessions read exactly
the current contact's digest row. Tenant-level digests are for tenant
admin/operator surfaces and are not inherited into contact sessions by default.
Digest rows are rebuilt on the consolidation cadence with a minimum interval,
so this block changes on the digest rebuild cadence rather than every turn.

The memory processor runs after query rewriting and before history compilation. It reads the effective `retrieval_query` metadata when present. If the rewrite source is `original` or metadata is absent, it uses the latest user message unchanged as the retrieval query. Rewrite gating stays in `QueryRewriter`; graph memory retrieval does not run rewrite logic.

It inserts ranked graph hits with labels, names, properties, provenance, and concise snippets. Memory content is inserted near the active turn so static prompt prefix caching remains stable.

The post-fusion reranker stage is always present in runtime retrieval. Its
default model selector is `noop`, which preserves fused candidate order. Setting
`memory.retrieval.reranker_model` to `provider:model`, such as
`cohere:rerank-v4.0-fast` or `zeroentropy:zerank-2`, switches the stage to a
provider-backed reranker.

For verified contact sessions, retrieval queries tenant knowledge and the
canonical verified contact memory scope inside the tenant. Storage lineage and
query trace records preserve both source tiers. The retrieval path does not read
tenant admin/operator memory or other-contact memory as implicit ancestors. If
there is no admitted contact for the session, retrieval uses tenant knowledge
only.

When `memory.retrieval.lineage_enabled` is true, retrieval records best-effort
lineage rows after ranking: tenant, contact, session, turn sequence, durable
turn id when known, node UID, rank, and timestamp. The write is fire-and-forget
and flag-dark by default, so normal retrieval does not wait on lineage
persistence.

Tenant knowledge query trace records are renderer-facing lineage views. They
record the original query, rewritten retrieval query, searched scopes, graph,
vector, lexical, and reranker legs, candidate counts, selected chunks/facts,
source tier, citations, tenant/contact/PII/ACL/source/label/freshness filters,
and per-stage latency. Query trace rows and lineage rows must redact provider
tokens, credential references that reveal secret material, full raw documents,
and contact points.

## Consolidation

Tenant consolidation is a scheduled maintenance pass. In cloud mode it is the
`Consolidate` Restate workflow. Locally and in eval it runs through the shared
`moa-memory-lifecycle` crate. The workflow is a thin durable wrapper; the memory
logic does not depend on Restate, so hermetic eval runs and scheduled
maintenance call the same code.

Consolidation v1 runs five deterministic operations:

- **Exact duplicate merge** groups active `Fact` nodes by `(tenant_id, contact_id, scope, fact_hash)`. The canonical is the earliest `valid_from` row with UID as the tiebreak. Other active rows are closed with a `SUPERSEDES` edge in the same direction as normal graph supersession: replacement/canonical `-> SUPERSEDES -> old`.
- **Anchored confidence decay** lowers confidence for idle facts. On first decay the current confidence is copied to `properties.base_confidence`; future runs recompute from that base instead of multiplying against the current value. This makes rerunning at the same `now` idempotent. Decay floors at the configured minimum and never deletes or invalidates a fact.
- **Contradiction sweep** groups active facts by `(tenant_id, contact_id, scope, subject, predicate)` only for explicit v1 update/contradiction predicates such as `cache_backend_conflict`, `deploy_target`, and `on_call_primary`. If a group contains multiple objects, the newest `valid_from` row wins with UID as the deterministic tiebreak, and older rows are superseded. Broad or multi-valued predicates such as preferences, contact email, dependency, owner, editor, `uses`, `is`, and `switched to` are not swept in v1 because recorded extraction can use them across unrelated facts. No LLM judge runs in v1.
- **Entity backfill** embeds active `Entity` nodes that lack vector rows when an embedder is available, and promotes edge-level `alias_mention` values into `properties.aliases` through the graph property-update operation.
- **Digest rebuild** renders deterministic standing contact and tenant summaries from active `Fact` nodes above the decay floor. Preference-like predicates render first, then other facts, newest first within each tier. The renderer truncates at whole lines using a chars/4 token estimate and stores the included source fact UIDs in `moa.memory_digests`. Contact sessions consume only the current contact digest.

The v1 pass deliberately does not do semantic near-duplicate merging, LLM-polished digest prose, episode building, scope-drift repair, or destructive expiry. `at_floor` is reported for future policy design, but floor-bound facts remain active unless another write supersedes them.

Successful consolidation appends a tenant-local `memory_updated` entry to
`learning_log`.

The lifecycle crate also owns the dark quality-scoring job. It joins
`moa.retrieval_lineage` to persisted task-segment outcomes and writes
Beta(1,1)-smoothed scores, `(1 + successes) / (2 + uses)`, back to
`moa.node_index.quality_score` with epsilon-guarded idempotent updates. If the
task-segment outcome source is unavailable, the job logs and reports a skip
without writing. Scheduling and pruning policy are deferred until live lineage
exists.

## Learning Relationship

Memory is one output of the broader learning loop:

```text
Task segments
  -> segment assessments
  -> experience_records
  -> experience_attributions
  -> learning_candidates
  -> learning_log after promotion
  -> skill ranking and graph memory consolidation
```

Graph memory describes current knowledge; `learning_log` explains how and when a learned update entered the system.
Memory candidates can be proposed from high-confidence resolved experiences,
but the first implementation does not auto-promote them into graph writes.
Promotion remains a consolidation or human-reviewed action so a single noisy
segment cannot mutate durable memory.
