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
| Retrieval fusion and context assembly | `moa-brain` |
| Credential references and secret retrieval | `moa-security` through the `CredentialVault` trait |
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

Provider credentials and account tokens must never be stored in request
metadata, tracing fields, graph node properties, graph edge properties,
knowledge metadata, or query trace rows.

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
- `KnowledgeChunk`: retrieval-sized consecutive block group with
  `chunk_hash = blake3(ordered block_hashes)`.

Block and chunk hashes are content identities, not database identities. They
let `moa-knowledge` diff parser output, reuse embeddings, tombstone deleted
chunks, and produce stable citations across re-syncs.

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

Full chunk text lives in `moa.knowledge_chunks`; graph properties stay compact
and citation-friendly. Graph properties must not contain provider tokens,
account tokens, raw credential material, or unbounded raw source payloads.

## Retrieval Contract

`moa-brain` owns retrieval assembly. When graph memory is enabled:

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
and citation metadata. Contact memory includes minimal provenance and
privacy-filtered summaries. There is no separate public answer endpoint for
tenant knowledge; normal agent/session answer generation consumes the assembled
context.

## Public Endpoints

Task 9 defines the public HTTP routes for this surface:

- `POST /v1/knowledge/link-token`
- `POST /v1/knowledge/exchange-token`
- `POST /v1/knowledge/sync`
- `POST /v1/knowledge/sync-status`
- `POST /v1/knowledge/sync-events`
- `POST /v1/knowledge/connections`
- `POST /v1/knowledge/objects`
- `POST /v1/knowledge/object`
- `POST /v1/knowledge/query-trace`
- `POST /v1/knowledge/webhooks/llamaparse`
- `POST /v1/knowledge/webhooks/reducto`
- `POST /v1/knowledge/webhooks/nango`
- `POST /v1/knowledge/webhooks/merge`

Authenticated routes inject tenant identity before calling Restate. Provider
webhook routes do not expose tenant reads; the orchestrator verifies provider
signatures or configured webhook secrets before trusting payload contents.

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
