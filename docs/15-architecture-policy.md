# 15 - Architecture Policy

_Type ownership and accepted architecture decisions._

## Type Placement

A shared Rust type has one canonical definition site. Other crates import that
type instead of defining a lookalike.

### `moa-core`

`moa-core` owns tenant primitives, platform RLS context, session/event DTOs,
shared errors, and trait surfaces. It does not depend on the memory
subsystem. Runtime configuration (`MoaConfig`, its per-domain sub-configs, and
the `EnvOverlay`) lives in the `moa-config` crate.

Consumers import category-owned paths such as
`moa_core::types::identifiers::TenantId`,
`moa_core::types::session::SessionMeta`, `moa_core::events::Event`,
and `moa_core::traits::SessionStore`. The crate
root allowlist is exactly `MoaError`, `Result`, and `WORKSPACE_ID`; wildcard,
prelude, and compatibility re-exports are forbidden. Domain-specific ports
such as knowledge discovery and contact OTP delivery stay in their owning
domain crates rather than moving into `moa-core`.

- IDs: `TenantId`, `StoragePartitionId` for storage partition internals, `UserId`, `SessionId`
- Platform RLS context: `RlsContext`
- Errors: `MoaError`, `Result`
- Session and event DTOs: `SessionMeta`, `SessionStatus`, `Event`,
  `EventRecord`, `EventStream`, `EventRange`, `EventFilter`
- Trait surfaces: `BrainOrchestrator`, `SessionStore`, `BlobStore`,
  `BranchManager`, `HandProvider`, `LLMProvider`, `ChannelAdapter`,
  `BuiltInTool`, `ContextProcessor`, `CredentialVault`
- Tool execution context: `ToolContext`

### Memory crates

| Crate | Owns |
|---|---|
| `moa-memory/types` | Memory-specific runtime scopes such as `MemoryScope` and `ScopeTier`, plus conversion into `moa_core::types::memory::RlsContext` at memory boundaries |
| `moa-memory/graph` | Relational graph-primary storage, `GraphStore`, `PostgresGraphStore`, node/edge labels, write intents, changelog rows, lexical index types |
| `moa-memory/vector` | Embedding and vector-index abstractions, `VectorStore`, `PgvectorStore`, `TurbopufferStore`, vector query/result DTOs |
| `moa-memory/pii` | Privacy classification and redaction clients, `PiiClassifier`, `PiiResult`, `PiiSpan`, `PiiCategory` |
| `moa-memory/ingest` | Ingestion DTOs, `IngestionVO`, fast memory writes, contradiction detection |

`moa_core::types::security::SensitivityClass` is the sole sensitivity type. It
is persisted on graph nodes and used across graph, vector, classifier,
governance, and retrieval boundaries without crate-local aliases or re-exports.

### Auth crates

`moa-auth` is a namespace folder, not a parent crate.

| Path | Owns |
|---|---|
| `moa-auth/authz-schema` | OpenFGA object, relation, tuple, and model-version constants |
| `moa-auth/authz` | OpenFGA client, authorization checks, transactional outbox, outbox poller |
| `moa-auth/providers` | Local API-key auth, disabled auth, builtin approvals, null token vault, provider bundle construction, first-party OAuth 2.1 authorization server (`oauth_as`) |
| `moa-auth/auth0` | Optional Auth0 and generic OIDC providers behind the `auth0` feature |
| `moa-auth/fga-bootstrap` | OpenFGA bootstrap binary |

### Brain and retrieval

`moa-retrieval` owns retrieval:

- `HybridRetriever` (`crates/moa-retrieval/src/retrieval/hybrid.rs`)
- query planning DTOs

`moa-brain` owns context compilation:

- context processors and pipeline stages

The `Reranker` trait retrieval consumes lives in `moa-providers`. Shared wire
DTOs for the public HTTP edge and orchestrator HTTP surface live in `moa-wire`.

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
| Retrieval type | `moa-retrieval` |
| Context pipeline type | `moa-brain` |
| Wire DTO for the HTTP surface | `moa-wire` |

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
| Composition code | Constructing concrete dependencies in `RuntimeDeps`, feature-gated bindings, background jobs, provider selection, and binding in-process implementations through `build_endpoint` |

Handlers may validate transport shape and reject unauthenticated or unauthorized
requests, but policy decisions after that point belong in an application or
domain layer. Repositories should never call Restate handlers, reach into global
context, or own product policy. Domain crates should not depend on
`moa-orchestrator` or on handler DTOs.

Do not add internal network services, RPC clients, or remote-service seams for
this effort. A future deployed split must be replaceable from composition code
without changing turn workflows, handler contracts, or domain tests.

The architecture checker enforces dependency kinds as well as source layout:
forbidden production directions cannot be hidden as ordinary build edges, and
explicit dev-only exceptions remain counted. It also constrains raw
`OrchestratorCtx` dependency reads in orchestrator objects, services, and
workflows. That scoped rule does not assert repository-wide elimination of the
context type.

Zero raw `OrchestratorCtx::current()` reads remain under those roots, and no
allowance may reintroduce one: a bare `current()` hands a caller the whole
dependency graph, which is exactly the coupling the rule exists to prevent.
Eight counted `current_*` reads remain as temporary, per-accessor exceptions:

| File | Accessor | Count | Why it is still there |
|---|---|---:|---|
| `objects/session/execution_runs.rs` | `current_graph_pool()` | 3 | Keyed Session admission runs three control-plane replay transactions; removing them needs constructor-injected admission persistence in `SessionImpl` |
| `objects/session/execution_runs.rs` | `current_config()` | 1 | Session-owned template planning reads model and execution configuration; same injection prerequisite |
| `workflows/experiment_run/target_execution.rs` | `current_config()` | 2 | Experiment-run targets select the internal model and compile against execution limits; needs constructor injection into `ExperimentRunImpl` |
| `workflows/experiment_trial_run/target_execution.rs` | `current_config()` | 2 | Same selection and compilation for trial targets; needs constructor injection into `ExperimentTrialRunImpl` |

Each entry is counted, not wildcarded: adding a ninth read fails the checker
even in an already-allowed file. Task 6.6 removes all eight by injecting the
dependencies through their implementation constructors, after which the
allowance list is empty.

Postgres DDL has one declared owner per logical top-level table family in the
`moa-migrations` manifest. New or removed tables must update that manifest in
the same change; `xtask check-migrations` rejects missing and stale owners.

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

### ADR 0003 - Accepted Category-Owner Splits And Growth Ratchets

Status: Accepted.
Date: 2026-07-24.

The architecture checker had drifted into a dead gate: a stale configured path
aborted the run before any coupling rule executed, so nothing was actually
enforced. Restoring it forced a choice for every budget that reality had already
exceeded — raise it, or claim a number the tree does not meet. Silently raising
budgets is how the gate died the first time, so the current shape is recorded
here explicitly instead.

Decisions:

1. The workspace is **51 packages** and **48 default members**. The category
   splits that produced this count are accepted, not debt: `moa-config`,
   `moa-wire`, `moa-retrieval`, and `moa-analytics-export` own configuration,
   wire DTOs, retrieval, and analytics export respectively, and the memory and
   auth namespaces are folders of separately-owned crates rather than parent
   crates. `moa-orchestrator` stays **one** crate — an earlier verified split
   into domain crates was reverted deliberately.
2. `moa-core` has **43 direct** and **46 transitive** workspace reverse
   dependencies. That fan-in is the cost of owning IDs, RLS context, events,
   errors, and trait surfaces in one place. It is accepted at this number; it is
   not a licence to add more.
3. `crates/moa-core/src/types/worker/state.rs` is **344 lines** because
   `WorkerInitialTask` now carries the inherited authenticated identity, which
   must travel with the task rather than be re-derived by the child.
4. Production LOC caps are enforced against **production** files. Large inline
   test modules move into child `tests.rs` modules beside their owner instead of
   inflating the owner's cap. The four caps held through that extraction: env
   overlay 1,664; edge routes 1,749; turn execution 1,535; worker commands 352.
5. Repository code owns SQL. Handlers, services, and workflows own transport,
   authorization, and orchestration. Extracting a repository is the remedy for a
   `DirectSql` finding; adding an allowance is not.

Consequences:

- The gate runs to completion and every rule reports, so later refactors are
  actually protected.
- The recorded numbers are ratchets, not targets: the checker fails when any is
  exceeded, and raising one requires a new decision record in this file stating
  what category boundary changed and why. "The number grew" is not a reason.
- The budgets are now honest, which means they will fail more often. That is the
  intended behavior.
- Configuration is self-validating: every configured scan root, allowance, LOC
  and symbol budget, sensitive consumer, and trace-manifest path is checked to
  exist before any rule runs, and a stale path fails with its owner and exact
  path instead of aborting the gate.

Revisit when a new crate is genuinely required by a category boundary, when
`moa-core` fan-in changes because a type moved to its domain owner, or when a
production file exceeds its cap for a reason other than accumulated inline
tests.
