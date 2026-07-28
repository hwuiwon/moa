# 21 — Tenant Knowledge Base

_Linked connector ingestion, parsing, graph writes, retrieval policy, and inspection._

## Purpose

Tenant knowledge base is tenant-owned source knowledge synced from external
systems such as file stores, knowledge bases, CRM, ticketing, chat, and custom
records. It is separate from contact memory and session memory:

- Tenant knowledge is synced through `moa-knowledge`, parsed into blocks and
  chunks, and written into tenant-scoped graph/vector memory.
- Contact memory is admitted end-user memory for one contact inside one tenant.
- Session memory ingestion remains the `Memory.ingest_documents` slow path for
  explicit session/document memory operations. Tenant connector sync must not
  overload `Memory.ingest_documents` or synthesize session turns.

At answer time, contact sessions use tenant knowledge plus admitted contact
memory by default when graph memory is enabled. If there is no admitted contact,
the default retrieval path uses tenant knowledge only.

## Owning Crates

| Concern | Owner |
|---|---|
| Linked-account domain, provider sync, parser abstraction, normalization, block/chunk identity, sync-run inspection | `moa-knowledge` |
| Restate service and workflow binding | `moa-orchestrator` |
| Public HTTP route translation | `moa-edge` |
| Graph labels, relational node/edge writes, SQL sidecars, and changelog rows | `moa-memory-graph` |
| Embeddings and vector rows | `moa-memory-vector` |
| Privacy classification and redaction | `moa-memory-pii` |
| Retrieval fusion and admission | `moa-retrieval` |
| Context assembly | `moa-brain` |
| Credential references and secret retrieval | `moa-auth-providers` through the `CredentialVault` trait, injected once by the orchestrator |
| Query-time citations, lineage, and audit sinks | `moa-lineage-*` |

`moa-memory-*` never imports Nango, Merge, LlamaParse, Unstructured, or Reducto.
Those providers stay behind `moa-knowledge` abstractions.

## Linked Connector Flow

`KnowledgeConnection` is one linked external account for one tenant. The
provider-backed flow is:

1. Tenant admin/operator requests a link token.
2. `moa-edge` forwards the request to the `Knowledge` Restate service.
3. `moa-orchestrator` calls `moa-knowledge`.
4. `moa-knowledge` calls a `LinkedIntegrationProvider` adapter for Nango or
   Merge.
5. The frontend completes the provider link flow and returns a public token.
6. `moa-knowledge` exchanges that token for provider account identity.
7. Provider credentials, API keys, and account tokens are stored through the
   credential vault. Knowledge rows store only credential references.

The whole link is one operation-fenced claim in `moa.knowledge_link_claims`,
keyed by `(tenant, operation_id)` and advanced by compare-and-swap through
`reserved -> credential_written -> finalized`, with
`compensating -> compensated` as the terminal failure path. The claim resolves
the owning `connection_uid` *before* any credential is written — a re-link keeps
the connection the upsert conflict target resolves to — and records the exact
previous-active and candidate references so compensation revokes only what this
operation wrote and restores only what it superseded.

A queued sync run is not evidence that the provider was called.
`moa.knowledge_sync_runs.provider_trigger_completed_at` is a separate write-once
boundary set after a successful dispatch and never rewritten by status updates.
A link may finalize only on a run it owns whose boundary is durable; a crash
between claiming the run and dispatching replays the exact idempotent trigger.
That is why the initial link uses a different provider call than an operator
re-sync: Nango's naturally idempotent `/sync/start` rather than the one-off
`/sync/trigger`, and for Merge a read-only, category-correct sync-status
reconciliation rather than the plan-gated, credit-consuming force-resync. Merge
readiness follows its documented rule — `status = DONE` or
`is_initial_sync = false`, skipping disabled models — and fails closed on failed,
paused, or unrecognized states. The Merge product category the operator selected
is carried through the exchange, validated against the linked integration's
declared `categories`, and used in every category-scoped versioned endpoint.

Tenant purge owns credential lifecycle: the `credentials_purged` stage drains
every stored version, its permitted audit projection, and the tenant's link
claims through bounded batches, looping until the owner reports nothing left.

Provider credentials and account tokens must never be stored in request
metadata, tracing fields, graph node properties, graph edge properties,
knowledge metadata, or query trace rows.

Signed Nango and Merge webhooks are provider CDC/sync signals, not raw data
ingestion payloads. After signature verification, MOA binds the provider event
to a tenant `KnowledgeConnection` by signed tenant/connection identifiers or by
the provider account identity, records the event idempotently, advances the
active sync run to `provider_synced` when the provider reports completion, and
dispatches the ingestion workflow. Duplicate deliveries do not enqueue another
workflow run, and ambiguous or unbound provider account events fail before local
state is written.

The provider abstraction is:

```rust
#[async_trait]
pub trait LinkedIntegrationProvider {
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken, Error>;
    async fn exchange_public_token(&self, req: ExchangePublicTokenRequest) -> Result<LinkedAccount, Error>;
    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync, Error>;
    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage, Error>;
    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent, Error>;
}
```

Nango is used for code-owned sync functions and record-cache cursor semantics.
Merge is used for normalized common models and hosted link UI. Test fakes are
allowed in tests but are not runtime providers.

## Parsing And Identity

`DocumentParser` is the parser abstraction. The native parser is the default:
plain text and structured record formats stay deterministic in MOA, while
local PDF and layout-aware file parsing uses the open-source `liteparse` Rust
crate instead of a custom parser. Configured external adapters are LlamaParse,
Unstructured, and Reducto.

```rust
#[async_trait]
pub trait DocumentParser {
    async fn parse(&self, input: ParseInput) -> Result<ParsedDocument, Error>;
}
```

All parsers emit the same structure:

- `KnowledgeObject`: one source-side object such as a file, page, ticket,
  message, CRM company, CRM contact, or knowledge-base article.
- `DocumentVersion`: one immutable parsed content version for the object.
- `DocumentElement`: parser output unit such as heading, paragraph, list item,
  table row, message, field, attachment, or `liteparse` text item with page,
  bounding-box, and OCR-confidence metadata when available.
- `KnowledgeBlock`: normalized atomic unit with
  `block_hash = blake3(normalized_text)`.
- `KnowledgeChunk`: retrieval-sized consecutive block group with content hash
  `chunk_hash = blake3(ordered block_hashes)` and occurrence identity
  `chunk_uid = blake3(document version, ordinal, ordered block_hashes)`.

Block and chunk hashes are content identities, not database identities and never
graph identities. They let `moa-knowledge` diff parser output, decide when an
embedding computation can be reused, and tombstone deleted chunks.

Occurrence identity is separate and mandatory. `chunk_uid` is the chunk's graph
node uid: `moa.knowledge_chunks.graph_node_uid` is NOT NULL and constrained equal
to `chunk_uid`, under a unique index. Two documents containing the same paragraph,
and two versions of one document, therefore own separate graph nodes, embeddings,
provenance edges, citations, and deletion targets. A document version may hold
several occurrences of identical text. Invalidation and deletion address persisted
occurrence uids for every active version of an object — never the latest version
alone, and never a uid recomputed from tenant plus content hash.

Signed parser completion webhooks are ingestion signals only after they bind to
tenant, connection, object, and an active sync run. A valid LlamaParse or
Reducto callback must include safe binding metadata such as `tenant_id`,
`connection_uid`, and either `object_uid` or `source_id`; MOA verifies that the
object belongs to the signed connection before recording the event. Accepted
completion callbacks record an object-scoped `parser_completion_received` step,
advance a waiting run to `provider_synced`, and enqueue local ingestion.
Malformed, ambiguous, or unbound callbacks fail with safe error messages and
must not echo provider tokens, parser secrets, or raw document content.

## Graph Contract

`moa-knowledge` writes graph deltas through `moa-memory-graph`. The graph labels
used for tenant knowledge are:

- `Source`: external system, linked connection, and source collection evidence.
- `Document`: one active or historical document version for a source object.
- `Chunk`: retrievable tenant knowledge chunk with citation metadata.
- `Fact`: extracted or normalized fact grounded in a chunk.
- `Entity`: source-side entity or normalized tenant entity.

Required edge relationships are:

- `HAS_DOCUMENT`: `Source -> Document`
- `HAS_CHUNK`: `Document -> Chunk`
- `EVIDENCES`: `Chunk -> Fact`
- `MENTIONS`: `Chunk -> Entity`
- `DERIVED_FROM`: contact groups or derived facts back to evidence objects.

`Chunk` nodes are per-occurrence: one node per `moa.knowledge_chunks` row, keyed
by that row's `chunk_uid`, carrying its `version_uid` in node properties.
`HAS_CHUNK`, `EVIDENCES`, and `MENTIONS` edges are therefore occurrence-specific,
while `Fact` and `Entity` nodes stay shared across the occurrences that evidence
them. Each occurrence also owns its own embedding row and vector association;
embedding computation may be reused between occurrences only when the complete
contextual input (document title, heading path, chunk text) and the embedding
model and version all match.

Full chunk text lives in `moa.knowledge_chunks`; graph properties stay compact
and citation-friendly. Graph properties must not contain provider tokens,
account tokens, raw credential material, or unbounded raw source payloads.

## Retrieval Contract

`moa-retrieval` owns retrieval fusion and admission; `moa-brain` owns context
assembly. When graph memory is enabled:

- Contact sessions retrieve tenant knowledge and admitted contact memory.
- Sessions without an admitted contact retrieve tenant knowledge only.
- Tenant knowledge and contact memory remain separate source tiers through
  ranking, dedupe, context assembly, lineage, and query trace output.
- The prompt context uses:

```text
<knowledge_context>
  <tenant_knowledge>...</tenant_knowledge>
  <user_memory>...</user_memory>
</knowledge_context>
```

Tenant knowledge includes source URI/title, document version, chunk identity,
and citation metadata. Hydration resolves one graph uid to exactly one
document-version occurrence, so a citation names the document the retrieved text
actually came from. Contact memory includes minimal provenance and
privacy-filtered summaries. There is no separate public answer endpoint for
tenant knowledge; normal agent/session answer generation consumes the assembled
context.

## Visibility And Access Control

Tenant knowledge admission has two modes, and the connector — not an operator —
decides which one applies. A `LinkedIntegrationProvider` declares a
`ProviderAclCapability`; `UniformlyPublic` produces a `TenantPublic` connection,
`NativeSnapshots` produces a `ProviderManaged` one. There is no default and no
caller-supplied override, and a re-link can never widen an existing
`ProviderManaged` connection back to `TenantPublic`.

`TenantPublic` is the old behavior: every synced document is visible to every
contact of the owning tenant, bounded only by the pinned agent knowledge policy
(which can disable knowledge retrieval, cap the retrieval budget, and set a PII
floor) and tenant RLS.

`ProviderManaged` reproduces the source system's own decision. Admission requires
all of:

- the object's ACL state is `current`;
- its `current_acl_snapshot_id` names a snapshot that is `complete` and whose
  `provider_revision` equals the object's recorded `acl_revision`;
- at least one `allow` entry in that snapshot matches one of the caller's
  principals;
- no `deny` entry in that snapshot matches any of them.

Anything missing, incomplete, stale, or revision-mismatched denies. A caller with
no resolved principals is denied. Tenant role and operator status do not bypass
this: an operator authorized to list a connection's control-plane metadata still
needs the source's own permission to read one chunk of its content.

Principals are stored only as keyed opaque fingerprints. A provider identity is
canonicalized to `namespace/kind/subject`, HMAC-SHA256'd with the tenant's
versioned ACL key (KMS-wrapped, in `moa.knowledge_source_acl_keys`), and encoded
as two key-version bytes plus the digest. No email address, phone number, or
provider label reaches a row, a log line, a trace, or a cache key. Because the key
version is inside the fingerprint, a rotation stops old entries from matching —
which fails closed.

The caller's principal set is resolved once per turn, durably, from the
authenticated session/contact identity plus verified bindings in
`moa.knowledge_source_principal_bindings` (direct) and
`moa.knowledge_source_principal_group_bindings` (one level of group/domain
expansion). It is never read from a request payload and never re-fetched inside a
retrieval leg. A provider "anyone with access" grant is bound once per connection
under the tenant-wide holder sentinel rather than fanned out per contact.

Enforcement is one shared SQL predicate (`moa_db::push_source_acl_predicate`)
applied by every path that can surface source content: Postgres lexical search
(primary and prefix fallback), pgvector KNN (single-stage and the Matryoshka
shortlist), the recursive graph walk's seed base case *and* every intermediate
hop, chunk hydration, and each context-window neighbour. Candidates from an
external vector backend, which answers outside Postgres, get one batched
admission check before fusion and before graph seeding. Tenant RLS remains
underneath as defense in depth.

Snapshots and their entries are immutable: `moa.knowledge_source_acl_snapshots`
and `moa.knowledge_source_acl_entries` have no `UPDATE` policy and no `UPDATE`
grant, so a permission set cannot be edited in place under an unchanged revision.
A permission change mints a new snapshot and moves the object's pointer
atomically.

Every snapshot, binding, and object-state change bumps the tenant's
`moa.knowledge_source_acl_epochs` counter. That epoch, together with the
aggregate principal-set fingerprint, is part of retrieval cache identity, so a
revocation invalidates warm result caches without any explicit cache plumbing. A
request whose ACL context was never resolved carries `SOURCE_ACL_EPOCH_UNRESOLVED`
and bypasses the cache entirely — an entry with no epoch could never be
invalidated.

Ingestion captures the ACL *before* the change-token and content-hash skip
fences, so an unshared folder stops being retrievable on the next sync pass
without re-parsing or re-embedding anything. A permission-bearing record whose
ACL could not be fully enumerated is recorded as `incomplete` — which hides it —
and only then raises a typed error.

Migration semantics: V000348 promotes nothing. Both shipped adapters are
permission-bearing, so every pre-existing connection becomes `ProviderManaged`
and every pre-existing object becomes `incomplete`. Content ingested before ACLs
were captured is invisible to everyone until a resync captures real permissions.

See [Security](08-security.md) for the cross-referenced policy.

## Public Endpoints

The public HTTP routes for this surface are:

- `POST /v1/knowledge/integrations`
- `POST /v1/knowledge/link-token`
- `POST /v1/knowledge/exchange-token`
- `POST /v1/knowledge/sync`
- `POST /v1/knowledge/sync-status`
- `POST /v1/knowledge/sync-events`
- `POST /v1/knowledge/connections`
- `POST /v1/knowledge/connections/source-selection`
- `POST /v1/knowledge/connections/disconnect`
- `POST /v1/knowledge/objects`
- `POST /v1/knowledge/object`
- `POST /v1/knowledge/query-trace`
- `POST /v1/knowledge/webhooks/llamaparse`
- `POST /v1/knowledge/webhooks/reducto`
- `POST /v1/knowledge/webhooks/nango`
- `POST /v1/knowledge/webhooks/merge`

Index rebuilds are exposed on the memory surface rather than the knowledge one,
because a rebuild covers the tenant's whole storage partition and not only its
synced documents:

- `POST /v1/memory/index-rebuild/start`
- `POST /v1/memory/index-rebuild/status`
- `POST /v1/memory/index-rebuild/cancel`
- `POST /v1/memory/index-rebuild/rollback`
- `POST /v1/memory/index-rebuild/finalize`

Each requires tenant-admin authority and is mirrored by an operator MCP tool
(`index_rebuild_start`, `index_rebuild_status`, `index_rebuild_cancel`,
`index_rebuild_rollback`, `index_rebuild_finalize`). A rechunk activates its
staged chunks, graph deltas, embeddings, ACL snapshot fingerprints, occurrence
identity, and provenance in one scoped transaction; see "Storage-partition index
rebuilds" in `docs/04-memory-architecture.md`.

Authenticated routes inject tenant identity before calling Restate. Provider
webhook routes do not expose tenant reads; the orchestrator verifies provider
signatures or configured webhook secrets before trusting payload contents.

The connect flow is integration-generic: `/v1/knowledge/integrations` lists the
integrations each enabled provider can connect (Nango reports its live project
catalog; Merge reports its unified-API categories), and the returned integration
id is the exact `connector` value the link-token flow accepts. Connect UIs
should render this list instead of hardcoding integration names.

Record content materializes provider-agnostically: inline `text`/`content`
payload fields ingest directly, downloadable URLs pass through to parsers, and
metadata-only records go through the provider's `fetch_record_content` hook
(Nango implements it via the Nango proxy with a per-integration strategy
registry — Google Drive today; adding an integration is one strategy module
plus a registry entry). Auth-walled viewer links such as Drive's `webViewLink`
are provenance only and are never fetched.

## Inspection And Observability

Tenant operators inspect ingestion through sync-run and object routes, not raw
database access. `KnowledgeSyncRun` records the overall attempt for one
`KnowledgeConnection`. `moa.knowledge_ingestion_steps` records the ordered
ingestion steps for each run and object:

```text
provider_triggered
provider_records_listed
object_change_checked
content_fetched
parse_submitted
parser_completion_received
parse_completed
normalized
blocks_diffed
chunks_diffed
embedded
graph_upserted
vector_indexed
contact_groups_derived
completed
```

Each step stores `status`, `started_at`, `ended_at`, `duration_ms`, safe
counters, safe summary, retry count, and typed `error_code`. Safe counters cover
records listed, records changed, records deleted, bytes fetched, parser pages,
parser items, blocks total/new/deleted, chunks total/new/deleted, embeddings
created/reused, graph nodes/edges upserted, vector rows upserted/deleted, and
contact-group memberships changed.

Users inspect query-time evidence through `POST /v1/knowledge/query-trace`.
The query trace explains the user query, rewritten retrieval query, searched
scopes, retrieval legs, candidate counts, selected chunks/facts, source tiers,
citations, filters, and per-stage latency. Query explanation is separate from
answer generation; normal sessions answer the user, while trace endpoints read
lineage and retrieval trace records after the fact.

MOA operators inspect traces and metrics through `moa-observability` and
`moa-lineage-*`. Required metrics include sync-run totals, record action
totals, ingestion-step duration, parse-job totals, chunk totals, embedding
totals, graph-write totals, retrieval duration, and retrieval hits by source
tier and leg. Traces, metrics, lineage, and audit rows must redact provider
tokens, credential material, full raw documents, and contact points.
