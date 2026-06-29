# 15 - Architecture Policy

_Type ownership and accepted architecture decisions._

## Type Placement

A shared Rust type has one canonical definition site. Other crates import that
type instead of defining a lookalike.

### `moa-core`

`moa-core` owns tenant primitives, platform RLS context, session/event DTOs,
shared errors, config, and trait surfaces. It does not depend on the memory
subsystem.

- IDs: `TenantId`, `StoragePartitionId` for storage partition internals, `UserId`, `SessionId`
- Platform RLS context: `RlsContext`
- Config and errors: `MoaConfig`, `MoaError`, `Result`
- Session and event DTOs: `SessionMeta`, `SessionStatus`, `Event`,
  `EventRecord`, `EventStream`, `EventRange`, `EventFilter`
- Trait surfaces: `BrainOrchestrator`, `SessionStore`, `BlobStore`,
  `BranchManager`, `HandProvider`, `LLMProvider`, `ChannelAdapter`,
  `BuiltInTool`, `ContextProcessor`, `CredentialVault`
- Tool execution context: `ToolContext`

### Memory crates

| Crate | Owns |
|---|---|
| `moa-memory/types` | Memory-specific runtime scopes such as `MemoryScope` and `ScopeTier`, plus conversion into `moa-core::RlsContext` at memory boundaries |
| `moa-memory/graph` | Relational graph-primary storage, `GraphStore`, `PostgresGraphStore`, node/edge labels, write intents, `PiiClass`, changelog rows, lexical index types |
| `moa-memory/vector` | Embedding and vector-index abstractions, `VectorStore`, `PgvectorStore`, `TurbopufferStore`, vector query/result DTOs |
| `moa-memory/pii` | Privacy classification and redaction clients, `PiiClassifier`, `PiiResult`, `PiiSpan`, `PiiCategory` |
| `moa-memory/ingest` | Ingestion DTOs, `IngestionVO`, fast memory writes, contradiction detection |

`PiiClass` lives in `moa-memory/graph` because it is persisted on graph nodes
and used for retrieval filtering. The classifier crate re-exports it but does
not define its own privacy class enum.

### Auth crates

`moa-auth` is a namespace folder, not a parent crate.

| Path | Owns |
|---|---|
| `moa-auth/authz-schema` | OpenFGA object, relation, tuple, and model-version constants |
| `moa-auth/authz` | OpenFGA client, authorization checks, transactional outbox, outbox poller |
| `moa-auth/providers` | Local API-key auth, disabled auth, builtin approvals, null token vault, provider bundle construction |
| `moa-auth/auth0` | Optional Auth0 and generic OIDC providers behind the `auth0` feature |
| `moa-auth/fga-bootstrap` | OpenFGA bootstrap binary |

### Brain and retrieval

`moa-brain` owns retrieval and context compilation:

- `HybridRetriever`
- query planning DTOs
- context processors and pipeline stages
- reranker public surface used by retrieval

### Placement Rules

| Type kind | Crate |
|---|---|
| ID newtype shared by two or more crates | `moa-core` |
| Trait surface shared by two or more crates | `moa-core` |
| Platform DB/RLS context | `moa-core` |
| Implementation of a `moa-core` trait | Implementing crate |
| Memory-specific runtime scope | `moa-memory/types` |
| Graph node, edge, or sidecar type | `moa-memory/graph` |
| Embedding or vector-index type | `moa-memory/vector` |
| Privacy classifier type | `moa-memory/pii` |
| Ingestion pipeline DTO | `moa-memory/ingest` |
| Retrieval or context pipeline type | `moa-brain` |

Anti-patterns:

- defining the same public type in two crates;
- putting graph-specific types in `moa-core` because another crate might need
  them later;
- adding compatibility aliases for superseded memory APIs;
- adding empty connector traits or clients before connector work is actively
  scheduled.

## Modular Monolith Boundary Policy

MOA remains one production binary. `moa-orchestrator` is the Restate transport,
workflow, and composition boundary for that binary; it is not a place to collect
domain rules or persistence details. Extraction readiness means domain logic can
move behind a different implementation at the composition root later, while
current production calls stay in-process.

Allowed responsibilities:

| Boundary | Owns |
|---|---|
| Restate handlers and workflows | Authentication context, authorization checks, DTO translation, `ctx.run` durability boundaries, Restate service/workflow calls, and transport-level telemetry |
| Application services | Use-case orchestration, business decisions, state transitions, idempotency, domain events, and calls to repositories or existing typed domain APIs |
| Repositories | SQL, row mapping, transactional persistence helpers, storage errors, and Postgres-specific query optimization |
| Domain crates | Stable domain models, traits, validation, policy types, reusable algorithms, and tests that should outlive the current Restate adapter |
| Composition code | Constructing concrete dependencies, feature-gated bindings, background jobs, provider selection, and wiring in-process implementations into `OrchestratorCtx` |

Handlers may validate transport shape and reject unauthenticated or unauthorized
requests, but policy decisions after that point belong in an application or
domain layer. Repositories should never call Restate handlers, reach into global
context, or own product policy. Domain crates should not depend on
`moa-orchestrator` or on handler DTOs.

Do not add internal network services, RPC clients, or remote-service seams for
this effort. A future deployed split must be replaceable from composition code
without changing turn workflows, handler contracts, or domain tests.

## Decision Records

Accepted decisions are immutable. Supersession should be recorded explicitly in
this file instead of editing history into a new shape.

### ADR 0001 - Envelope Encryption Deferred To v1.1

Status: Accepted.
Date: 2026-05-05.
Supersedes: original M21 design.

The original M21 design specified per-tenant KEK plus per-fact DEK envelope
encryption. After the PII model changed, ingestion redacts sensitive text before
embedding and graph storage, so original PHI no longer persists in the
canonical store.

Decision: defer envelope encryption. v1 relies on redaction at ingestion as
the privacy boundary. Hard-purge through privacy erasure is the erasure path.

Consequences:

- ingestion and erasure stay simpler;
- v1 has less KMS dependency surface;
- a PII service miss can still put cleartext into persisted graph fields;
- tenants cannot yet opt into defense-in-depth encryption for restricted
  slices.

Mitigations:

- redaction-bypass tests assert PHI patterns from ingestion input do not persist
  in the canonical store;
- classifier contract tests fail if known PHI is returned unchanged;
- `pii_class` remains on every node so encryption can later target restricted
  rows without reclassification.

Revisit when a tenant needs restricted-row encryption, a redaction miss reaches
storage, an audit requires per-record key destruction, or multi-tenant
deployment requires per-tenant KMS isolation.

### ADR 0002 - Auth Architecture

Status: Accepted.
Date: 2026-05-11.

MOA needs identity, authorization, and security audit as first-class platform
capabilities. Self-hosted single-tenant deployments must work without external
identity SaaS, while multi-tenant SaaS can opt into Auth0/OIDC.

Decisions:

1. OpenFGA self-hosted, Postgres-backed, is the default authorization engine.
   One FGA store is used per deployment; tenants are relations in the model.
2. `AuthProvider`, `TokenVaultProvider`, and `AsyncAuthzProvider` live in
   `moa-core::traits`.
3. Defaults are local API keys, `NullTokenVaultProvider`, and builtin approvals.
4. Auth0/OIDC and Auth0 Token Vault are optional provider implementations.
5. Agents are first-class principals with subject identity `agent:<uuid>`.
6. `moa-edge` injects trusted `X-Moa-*` identity headers, and the orchestrator
   trusts them. The handler port must stay internal-only.
7. Product state and OpenFGA tuples are synchronized through the transactional
   outbox.
8. The public HTTP edge crate is `moa-edge`; the OCSF security-event crate is
   `moa-ocsf`.

Consequences:

- Self-hosted deployments do not require Auth0.
- Auth0 FGA remains a compatible managed swap-in option.
- Agents avoid service-account-under-user delegation ambiguity.
- Operating OpenFGA adds a local service.
- Exposing orchestrator port 9080 bypasses the trusted-header boundary.
- Security-event audit and lineage audit remain separate planes.

Revisit when a customer forbids API keys, OpenFGA becomes a scale bottleneck,
identity propagation must be cryptographically signed between services, or a
third audit event format becomes required.
