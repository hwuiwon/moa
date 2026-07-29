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
`moa-memory-graph` to write them. Required relationships map onto the canonical
`EdgeLabel` vocabulary in `crates/moa-memory/graph/src/edge.rs`: source-to-document
and document-to-chunk containment are stored as `CONTAINS`, chunk-to-fact evidence
and derived contact groups or facts pointing back to source evidence are stored as
`DERIVED_FROM`, and chunk-to-entity mentions are stored as `MENTIONED_IN`
(`moa-knowledge` translates its connector-facing relationship names to these
labels during ingestion). Full chunk text is stored in
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

Embedder selection is deployment/process configured through one
`provider:model` selector, for example `cohere:embed-v4.0` or
`gemini:gemini-embedding-2`. The resulting vector identity (model, version, and
dimensions) is pinned per tenant storage partition. Those models use
incompatible vector spaces, so switching the process configuration requires
re-embedding each affected partition before retrieval can safely use the new
model. That re-embedding is a first-class durable operation rather than an
operator script: see "Storage-partition index rebuilds" below.
Embedding construction never falls back across providers or models:
ingestion and retrieval each build the selected provider with their respective
role, and a failure stays within that vector space. Credential changes are
startup-only configuration because the process runtime and its provider client
are installed once.
### Storage-partition index rebuilds

Changing the embedder, changing the chunker, or recovering a corrupt index all
mean recomputing a whole storage partition. `moa.knowledge_rebuild_operation`
and its companions (V000351) make that a resumable operation instead of a
bespoke script.

A rebuild builds a *candidate generation* while the partition keeps serving the
generation it already has. Candidate vectors are written to
`moa.knowledge_rebuild_candidate_vector`, a table no retrieval leg reads, so a
shadow result cannot reach retrieval, ranking, hydration, lineage, or citations
even if a filter is forgotten. Exclusion is structural, not a predicate.

The properties the schema enforces, rather than the application:

* At most one nonterminal operation per partition, and exactly one `active`
  generation per partition, both partial unique indexes.
* Every lifecycle transition is a compare-and-swap on
  `(operation_uid, owner_token, lifecycle)`. A replayed Restate step observes
  `AlreadyApplied`; a foreign writer is refused and told what it observed.
* Progress is a keyset checkpoint that only advances, and `vectors_rebuilt` is
  recounted from the candidate table rather than incremented, so a replayed
  batch cannot inflate it.
* Activation is one compare-and-swap on `moa.knowledge_active_generation`, in
  the same transaction that promotes the candidate vectors into
  `moa.embeddings` and enqueues the external-backend outbox rows.

**Partition-wide, not chunk-only.** Facts, incidents, entities, and knowledge
chunks share one vector space, so a rebuild reconstructs the authoritative
embedding input for every label and fails closed on any it does not recognize.
The reconstruction rules are not uniform: a `Chunk` embeds its contextual form
(document title, heading path, then body — `contextual_chunk_embedding_input` in
`moa-core`), an `Entity` embeds its normalized name rather than its display
name, and the ingest-path labels embed `properties_summary->>'summary'`. The
Turbopuffer sync's `search_text` is the BM25 body and is *not* the embedding
input; rebuilding from it would silently produce a different vector space.

**Validation and rollback.** A complete candidate generation is scored by
bounded shadow queries that compare its top-K neighbors against the served
generation's, reusing only the pure overlap rule from the backend-promotion
engine — none of that engine's dual-read serving path. Below the 0.95 bar the
candidate is abandoned and the old generation was authoritative throughout.
After activation the prior generation is retained for explicit rollback;
finalization discards it, and only then is the retired contract unreadable.

**Rechunk** runs on the same generation state machine. It stages six members per
document version — chunks, graph deltas, embeddings, ACL snapshot fingerprints,
occurrence identity, and provenance — and refuses to activate until all six are
present, then applies document, chunk, graph, vector, changelog, outbox, and the
generation pointer in one scoped transaction. Staged ACL state holds only keyed
`SourcePrincipalFingerprint` hex; a provider principal never enters durable
rebuild state.

Ordinary vector writes are fenced while `reembed_state = 'in_progress'`. A write
that landed mid-build would either miss the census and vanish at activation, or
survive in the retired model's space; both are undetectable downstream, so
writers fail fast and retry after the rebuild finishes.

Gemini Embedding 2 is exposed as a text-only `Embedder` today; its API supports
multimodal inputs, but MOA needs a separate multimodal chunker and embedder
trait before image, audio, video, or PDF chunks are indexed.

Gemini Embedding 2 does not use a `task_type` request field. MOA encodes
asymmetric retrieval through role-specific prompt prefixes inside the embedder:
ingestion-side embedders use the document prefix and retrieval-side embedders
use a search-query prefix. Session ingestion constructs one process-shared
ingestion provider and reuses that exact client for slow-path facts, fast-path
remember/supersede/incident writes, and entity blocking. Explicit-pool helpers
build once per invocation and likewise share that client between fact and entity
work.

Indexes are write-incremental. There is no user-facing rebuild-index command for graph memory.

## Ingestion

Session and contact memory enter the graph through two routes:

- **Slow path**: `moa-memory-ingest` processes longer source text or turns through the ingestion VO. It chunks content, extracts facts/entities, classifies privacy, writes nodes and edges, embeds retrievable records, and records contradictions.
- **Fast path**: short observations use remember/forget/supersede APIs for direct graph writes with the same scope and privacy controls.

When the selected embedder is disabled or its selected-provider credential is
missing, runtime construction emits one structured warning. Slow ingestion
continues in explicit no-vector mode, preserving graph facts without embedding
bytes or model identity. Vector-producing fast remember, supersede, and incident
writes instead return a dedicated configured-embedder-unavailable error; fast
forget and privacy deletion remain available because they do not create vectors.
Invalid selectors, models, dimensions, or provider-client construction are
configuration errors, not no-vector downgrades.

Tenant knowledge ingestion is a third route owned by `moa-knowledge`, not by
`Memory.ingest_documents`. It links external accounts through Nango or Merge,
triggers provider sync, lists changed records, fetches content, parses records
and files through `DocumentParser`, derives `KnowledgeBlock` and
`KnowledgeChunk` identities, computes graph deltas, and writes tenant-scoped
knowledge through `moa-memory-graph` and `moa-memory-vector`. The session
memory slow path may continue to accept explicit documents, but connector sync
must not synthesize session turns or overload `Memory.ingest_documents`.

`KnowledgeBlock` content identity is `block_hash = blake3(normalized_text)`.
`KnowledgeChunk` content identity is `chunk_hash = blake3(ordered block_hashes)`;
these let ingestion diff changed documents, decide when an embedding computation
can be reused, and tombstone deleted source content.

Chunk *occurrence* identity is separate: `chunk_uid` derives from the document
version, the ordinal, and the content seed, and it is the chunk's graph node uid
(`moa.knowledge_chunks.graph_node_uid` is NOT NULL and constrained equal to it).
Equal text in two documents therefore never collapses into one graph node,
embedding, citation, or deletion target, and one graph uid hydrates exactly one
document-version occurrence. Content hashes stay for dedupe and diffing only.

Re-observation reinforces instead of dropping. When the contradiction detector
routes an extracted fact as a duplicate of an existing node (or the fast-path
`memory_remember` restates one), ingestion confirms the survivor: confidence
steps toward a cap, the `base_confidence` decay anchor clears so the next decay
re-anchors from the boosted value, and `last_accessed_at` advances. Combined
with anchored decay this makes memory use-it-or-lose-it: restated facts stay
hot, one-off mentions fade. The slow path records the boost with the same-turn
dedup row so Restate replays do not double-boost.

Slow-path fact extraction is behind the `FactExtractor` seam. The heuristic extractor remains the default and journal-safe fallback. Environments can opt into provider-backed `ModelFactExtractor` with `memory.extraction.enabled`; model selection, credentials, and configured chat-model failover come from the shared `moa-providers` config path, while memory prompts, parsing, and prompt versions stay in `moa-memory-ingest`. Eval replay uses recorded extraction fixtures so the natural transcript lane stays hermetic after live recording.

The model extractor may emit an optional `event_time` when the transcript
states when a fact became true ("I moved to Denver last August"). A stated,
non-future event time becomes the fact node's `valid_from`, so recency ranking
and as-of reads reflect event time rather than ingestion time. Event time is
not part of the fact identity hash, and malformed values degrade to the turn
instant instead of failing extraction. Prompt v3 adds only this optional key,
so recorded v2 fixtures remain replayable.

PII classification runs before redaction, embedding, or any durable memory
write. A successful classification may redact sensitive spans and records the
resulting privacy class. An abstaining classifier returns a retryable error and
writes nothing; unclassified plaintext is never merely tagged and admitted.

Privacy erasure follows attribution across every version of a subject's memory,
including active, invalidated, expired, and superseded rows. It removes the
attributable graph nodes and edges, vector projections, retrieval lineage, and
audit-linked memory closure rather than limiting deletion to the active
retrieval view. The same typed closure reaches subject-derived experience
records, experience attributions, learning candidates, contribution rows, and
artifact revisions. Historical bitemporal visibility does not override an
erasure request.

## Context Pipeline Integration

`MemoryAdmissionPolicy` is the shared authorization and visibility gate for
prompt injection and the `memory_search` and `memory_navigate` tools. All three
surfaces apply the same admitted scope before graph, lexical, or vector reads;
tool execution cannot widen what the prompt path may see. Postgres row-level
security remains defense in depth beneath this application policy, not a
substitute for it. A contact session may receive tenant knowledge and admitted
memory for its current contact only. Tenant operational/admin memory and every
other contact's memory remain outside that boundary.

The standing digest processor runs after query rewriting and before graph-memory
retrieval when `memory.digest.enabled` is true. Contact sessions read exactly
the current contact's digest row. Tenant-level digests are for tenant
admin/operator surfaces and are not inherited into contact sessions by default.
Digest rows are rebuilt on the consolidation cadence with a minimum interval,
so this block changes on the digest rebuild cadence rather than every turn.

The memory processor runs after query rewriting and before history compilation. It reads the effective `retrieval_query` metadata when present. If the rewrite source is `original` or metadata is absent, it uses the latest user message unchanged as the retrieval query. Rewrite gating stays in `QueryRewriter`; graph memory retrieval does not run rewrite logic.

It inserts ranked graph hits with labels, names, properties, provenance, and concise snippets. Memory content is inserted near the active turn so static prompt prefix caching remains stable.

The post-fusion reranker stage is always present in runtime retrieval. Its
default model selector is `cohere:rerank-v4.0-fast`; it falls back to `noop`,
which preserves fused candidate order, with a warning when the provider API key
is absent. Setting `memory.retrieval.reranker_model` to another
`provider:model`, such as `zeroentropy:zerank-2`, or to `noop` switches the
stage explicitly.

For verified contact sessions, retrieval queries tenant knowledge and the
canonical verified contact memory scope inside the tenant. Storage lineage and
query trace records preserve both source tiers. The retrieval path does not read
tenant admin/operator memory or other-contact memory as implicit ancestors. If
there is no admitted contact for the session, retrieval uses tenant knowledge
only.

Tenant knowledge from a permission-bearing connector is additionally admitted
under the source system's own ACL. `RetrievalRequest` carries a
`SourceAclContext` — the caller's bounded, canonical principal set as keyed
opaque fingerprints plus the tenant's source-ACL epoch — resolved once per turn
from authenticated identity and verified bindings, never from a request payload
and never re-fetched inside a leg. `GraphStore::expand_seeds` takes it explicitly
so the shared predicate applies to the seed base case and every recursive hop;
the lexical, pgvector, hydration, and context-window paths carry the same
predicate, and an external vector backend's candidates receive one batched
Postgres admission check before fusion. Tenant row-level security remains
underneath as defense in depth. See
[Tenant Knowledge Base](21-tenant-knowledge-base.md) for the admission rule and
its failure modes.

When `memory.retrieval.lineage_enabled` is true, retrieval records best-effort
lineage rows after ranking: tenant, contact, session, turn sequence, durable
turn id when known, node UID, rank, and timestamp. The write is fire-and-forget,
so normal retrieval does not wait on lineage persistence.
`memory.retrieval.lineage_sample_rate` (default `1.0`) caps lineage write cost
at scale: sampling is deterministic per `(session, turn)` — hashing, not
randomness — so a given turn either always records lineage or never does at a
fixed rate, and Beta-smoothed quality scores still converge on the sample.

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

Consolidation v1 runs six deterministic operations:

- **Exact duplicate merge** groups active unsealed `Fact` nodes by normalized `(tenant_id, contact_id, scope, subject, predicate, object)` — fact content, not the summary-bearing `fact_hash`, so the same fact restated in different words still merges. The canonical is the earliest `valid_from` row with UID as the tiebreak. Other active rows are closed with a `SUPERSEDES` edge in the same direction as normal graph supersession: replacement/canonical `-> SUPERSEDES -> old`. Restricted/PHI facts are excluded because content-dependent maintenance must never interpret their fixed redaction projection as plaintext.
- **Anchored confidence decay** lowers confidence for idle facts. On first decay the current confidence is copied to the typed `moa.node_index.base_confidence` sidecar; future runs recompute from that base instead of multiplying against the current value. Confidence maintenance never rewrites dynamic or sealed node content. This makes rerunning at the same `now` idempotent. Decay floors at the configured minimum and never deletes or invalidates a fact.
- **Contradiction sweep** groups active unsealed facts by `(tenant_id, contact_id, scope, subject, predicate)` only for explicit v1 update/contradiction predicates such as `cache_backend_conflict`, `deploy_target`, and `on_call_primary`. If a group contains multiple objects, the newest `valid_from` row wins with UID as the deterministic tiebreak, and older rows are superseded. Broad or multi-valued predicates such as preferences, contact email, dependency, owner, editor, `uses`, `is`, and `switched to` are not swept in v1 because recorded extraction can use them across unrelated facts. Restricted/PHI facts are excluded, and no LLM judge runs in v1.
- **Idle expiry** closes active `Fact` nodes that sit at the decay floor AND
  have not been accessed for `expire_idle_days` (default 180; `0` disables).
  The close is a bitemporal invalidation with reason `expired_idle` — history
  and as-of reads keep working — so the pass bounds the active retrieval set
  per tenant without destroying anything. It runs after decay so it sees
  post-decay confidence.
- **Entity backfill** embeds active unsealed `Entity` nodes that lack vector rows when an embedder is available, and promotes edge-level `alias_mention` values into `properties.aliases` through the graph content-update operation. Restricted/PHI entities are excluded from both operations.
- **Digest rebuild** renders deterministic standing contact and tenant summaries from active unsealed `Fact` nodes above the decay floor. Preference-like predicates render first, then other facts, newest first within each tier. Restricted/PHI facts are excluded rather than materializing their redaction placeholders. The renderer truncates at whole lines using a chars/4 token estimate and stores the included source fact UIDs in `moa.memory_digests`. Contact sessions consume only the current contact digest.

The v1 pass deliberately does not do semantic near-duplicate merging, LLM-polished digest prose, episode building, scope-drift repair, or destructive (hard-delete) expiry. `at_floor` is reported alongside `expired_idle`; floor-bound facts inside the idle window remain active unless another write supersedes them.

Consolidation writes no `learning_log` entry. The tenant-wide `memory_updated`
row it used to append carried no per-subject provenance, so it could be neither
erased nor explained, and nothing read it; its counts live on the returned
consolidation report and in metrics instead.

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

Every arrow above is a real foreign key, not a convention. `learning_candidate_source`
and `learning_log_source` hold one typed column per referent kind with a
composite key carrying the partition, which is what lets a privacy erasure walk
the chain in reverse: delete a subject's memories and the learning distilled
from them is reached and removed too, instead of surviving as an orphaned
conclusion whose evidence is gone.

Which review contract a candidate offers is `proposal_kind`, kept separate from
`candidate_type` (the target domain). Only `skill_draft` and `skill_rollback`
have a materializer and can be accepted; memory, policy, prompt, and eval
observations live on advisory or authoring lifecycles whose only exit is
dismissal. The database enforces both the legal `(kind, status)` pairs and the
legal transitions, so an advisory item cannot be walked to `promoted` one
legal-looking step at a time.

**Scope note.** Reverse-derived erasure covers learning-derived rows only. It
does not claim raw session-event, attachment, blob, or archive erasure — those
have their own owners and their own paths. See the scope fence in
`docs/08-security.md#learning-derived-erasure` before assuming coverage.
