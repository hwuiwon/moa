# 01 — Architecture Overview

_System model, trait map, data flow, and workspace layout._

## System Model

```text
Clients
  CLI | REST/gateway | Telegram/Slack/Discord
        |
        v
Runtime boundary
  Runtime: `moa-orchestrator` Restate handler service
        |
        v
Brain and execution
  Context pipeline -> provider router -> LLM
  Tool router -> built-ins / hands / MCP
  Sub-agent dispatch -> Restate SubAgent virtual objects
        |
        v
Product data in Postgres / Neon
  sessions, events, pending_signals, context_snapshots
  task_segments, segment analytics materialized views
  graph nodes, graph edges, sidecar indexes, pgvector embeddings
  tenant_intents, global_intent_catalog, learning_log
  analytics.turn_lineage, analytics.scores, compliance audit tables
  security_events
        |
        v
Learning loop
  segments -> resolution scores -> learning log
  learning log -> intent proposals, skill ranking, memory consolidation
```

Restate owns durable cloud execution. Postgres owns product-visible data. Graph memory is the canonical memory source, with sidecar and vector indexes maintained by graph writes.

MOA's enterprise boundary is the tenant. Runtime operators can run local mode for
development and incident response, but the product model assumes organizations
need governed execution, audit trails, tenant-owned learning, and clear rollback
paths.

## Tenant Model

```text
Platform
  -> Tenant (team)
       -> Users
       -> Workspaces
       -> Tenant intent taxonomy
       -> Tenant learning log
       -> Workspace memory
       -> Workspace skills ranked by tenant-level outcomes
       -> Lineage, analytics, and optional compliance evidence
```

Intent taxonomies are tenant-scoped because teams tend to repeat work patterns across projects. Memory and skills remain workspace-scoped, but ranking signals aggregate at tenant level.

## Core Traits

Current trait definitions live under `crates/moa-core/src/traits/` and
`crates/moa-core/src/traits/mod.rs`; shared DTOs live under
`crates/moa-core/src/types/`.

| Trait | Purpose | Main implementations |
|---|---|---|
| `BrainOrchestrator` | Start, resume, signal, list, observe sessions; schedule background work | Restate services/objects through `moa-orchestrator` |
| `SessionStore` | Append-only event log, sessions, pending signals, snapshots, task segments, analytics, skill rates | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `FileBlobStore` |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `HandProvider` | Provision, execute, pause/resume, destroy hands | local, Docker, Daytona, E2B |
| `LLMProvider` | Provider completion interface | Anthropic, OpenAI, Gemini through `moa-providers` |
| `EmbeddingProvider` | Shared embedding interface | OpenAI embedding, Cohere v4, Gemini embedding, and test/mock adapters |
| `PlatformAdapter` | Gateway inbound/outbound normalization | Telegram, Slack, Discord |
| `BuiltInTool` | Built-in tool execution | memory/search/web and other built-ins |
| `ContextProcessor` | One stage in context compilation | identity, instructions, tools, skills, query rewrite, memory, history, runtime context, compactor, cache |
| `CredentialVault` | Secret storage and retrieval | local encrypted vault; environment-backed MCP vault |
| `LineageHandle` | Transport-neutral lineage capture | null handle, async sink, OTel bridge |

Runtime entrypoints share these seams through the Restate-backed orchestrator.
Phase 1 auth work adds `AuthProvider`, `TokenVaultProvider`, and
`AsyncAuthzProvider` to `moa-core::traits`; see ADR-0002.

## Runtime Modes

### Cloud

`moa-orchestrator` exposes Restate handlers:

- Virtual objects: `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO`
- Services: `Health`, `SessionStore`, `IntentManager`, `LLMGateway`, `ToolExecutor`, `WorkspaceStore`
- Workflows: `Consolidate`, `IntentDiscovery`

`Session` is the durable actor for one session key. It queues messages, calls `run_turn`, tracks the active task segment, records tool/skill usage, scores resolution, and writes learning entries. `SubAgent` is the same actor pattern for delegated work with depth and budget limits.

### Thin Clients

`moa-cli` and `moa-runtime` call the configured Restate ingress through
`moa-orchestrator-client`. Client paths do not embed an orchestrator process.
Local development uses the same Restate-backed cloud runtime through `make dev`.

## Turn Data Flow

```text
User message
  -> SessionStore emits `UserMessage`
  -> Session VO prepares a turn
  -> Context pipeline runs
       1 identity
       2 instructions
       3 tools
       4 skills
       5 query_rewrite (when enabled)
       6 memory
       7 history
       8 runtime_context
       9 compactor
       10 cache
  -> Query rewrite may mark `is_new_task`
  -> SegmentTracker opens or rolls a task segment
  -> IntentClassifier compares segment text to active tenant intents
  -> LLM response is streamed/collected
  -> Tool calls route through ToolExecutor and ToolRouter
  -> BrainResponse and tool events are persisted
  -> Segment counters are updated
  -> ResolutionScorer scores completed or idle segments
  -> LearningEntry rows record resolution, intent classification, skill, or memory learning
```

If query rewriting is disabled, stage 5 is omitted and the remaining processors still report their configured stage numbers.

## Storage Overview

| Area | Store | Notes |
|---|---|---|
| Session metadata and events | Postgres | `sessions`, `events`, `pending_signals`, `context_snapshots` |
| Task segmentation | Postgres | `task_segments`, segment baselines, skill resolution rates, intent transitions |
| Graph memory | Postgres | Nodes, edges, sidecar indexes, changelog, and RLS-protected scope state |
| Memory vectors | Postgres | pgvector embeddings for graph retrieval |
| Tenant intents | Postgres | `tenant_intents` and `global_intent_catalog` |
| Learning audit | Postgres | `learning_log` append-only rows with bitemporal validity |
| Cloud orchestration state | Restate | VO/workflow state and journals, not product record |
| Optional checkpoints | Neon | branch manager for database checkpoints |
| Security events | Postgres and S3 | OCSF v1.3 events in `security_events`, shipped to tenant audit buckets |

## Auth Layer

ADR-0002 is the canonical decision record for MOA's identity,
authorization, naming, and security-event audit posture.

### Identity

`moa-edge` validates API keys by default, or OIDC tokens in Auth0/OIDC modes,
then injects `X-Moa-Identity-Type`, `X-Moa-Identity-Id`,
`X-Moa-Tenant-Id`, `X-Moa-Acting-On-Behalf-Of`, and
`X-Moa-Api-Key-Id` headers before forwarding to the orchestrator. The
orchestrator trusts these headers, so the Restate handler port (`9080`) must be
network-isolated in production; see
[`docs/operations/edge-network-isolation.md`](operations/edge-network-isolation.md).

Local deployments use the zero-dependency provider bundle by default:
`LocalAuthProvider`, `NullTokenVaultProvider`, and
`BuiltinAsyncAuthzProvider`. Builtin approvals are documented in
[`docs/operations/builtin-approvals.md`](operations/builtin-approvals.md).

### Authorization

OpenFGA, backed by Postgres, is the default authorization engine. The v1
authorization schema lives in `moa-authz-schema` as Rust constants. Handlers
call `require_authz(authz, identity, object_type, object_id, relation)` at the
behavior boundary; there are no procedural macros or implicit handler guards.

### Audit

OCSF v1.3 security events are written synchronously to a Postgres
`security_events` table. The existing `services/audit-shipper` service is
extended in P1.10 to ship those events to S3 with per-tenant bucket routing and
Object Lock compliance mode.

## Eval And Dashboards

Lineage records are captured through the hot-path `LineageHandle` bridge and
written asynchronously to `analytics.turn_lineage`. Eval, online-judge, and
human-review scores use the same sink via `LineageEvent::Eval(ScoreRecord)` and
land in `analytics.scores`, keyed by turn, session, or dataset replay item.

`moa eval datasets register` stores replay datasets in
`analytics.eval_datasets` and `analytics.eval_dataset_items`. `moa eval replay`
emits score records with a shared `run_id`, while `moa eval scores` and
`moa eval compare` read directly from `analytics.scores`.

Grafana dashboards live in `dashboards/grafana/` and Prometheus alert rules live
in `ops/prometheus/alerts/`. Import the dashboards with a Postgres datasource
named `DS_POSTGRES` and a Prometheus datasource named `DS_PROMETHEUS`; the
workspace selector is populated from `analytics.turn_lineage`.

## Compliance Audit Tier

Compliance audit is an opt-in superset of the engineering lineage tier. A row
in `analytics.compliance_workspaces` enables workspace-local BLAKE3 chain links
on `analytics.turn_lineage`, periodic Merkle roots in `analytics.audit_roots`,
PII pseudonymization side data in `pii_vault`, and DSAR tooling through
`moa lineage export`, `moa lineage verify`, and `moa lineage erase`. Workspaces
that are not enabled keep the L01-L03 behavior and store `prev_hash = NULL`.

Audit bucket bootstrap lives in `scripts/bootstrap-audit-bucket.sh`. Buckets
must be created with Object Lock enabled at creation time; production uses
Compliance mode and development uses a separate bucket, usually Governance mode
with short retention. Signing keys are local PKCS#8/seed files for development
and should be HSM-backed KMS Ed25519 keys in production. Switching signing keys
starts new windows with the new label; old verifying keys remain required for
old audit roots.

**ATTESTATION GATE - DO NOT REPRESENT THIS AS COMPLIANCE EVIDENCE TO REGULATORS
OR CUSTOMERS UNTIL EXTERNAL CRYPTOGRAPHIC REVIEW IS COMPLETE.** The
`ct-merkle` crate is explicitly not audited by its authors. `moa-lineage-audit`
must receive external cryptographer or appsec review before DSAR exports,
regulator responses, audit attestations, or certifications rely on this layer as
compliance-grade evidence. Internal debugging and forensics may use it before
that review. The review must cover BLAKE3 canonicalization and chain extension,
Ed25519 key handling, Merkle inclusion and consistency proof construction, PII
crypto-shredding semantics, S3 Object Lock configuration, timestamp discipline,
and replay resistance on the verify path.

## Workspace Layout

| Crate | Role |
|---|---|
| `moa-core` | Shared types, traits, config, events, analytics helpers |
| `moa-brain` | Context pipeline, query rewrite, segment helpers, intent classifier, resolution scoring |
| `moa-session` | Postgres session store, event log, task segments, intents, learning log |
| `moa-memory/graph` (`moa-memory-graph`) | Graph-memory SQL sidecars, RLS, changelog, and AGE projection helpers |
| `moa-memory/ingest` (`moa-memory-ingest`) | Slow-path graph ingestion and fast memory write APIs |
| `moa-memory/pii` (`moa-memory-pii`) | PII classification and privacy helpers |
| `moa-memory/vector` (`moa-memory-vector`) | Graph-memory vector storage abstraction and pgvector backend |
| `moa-lineage/core` (`moa-lineage-core`) | Lineage records and score record types |
| `moa-lineage/sink` (`moa-lineage-sink`) | Async lineage sink writers |
| `moa-lineage/otel` (`moa-lineage-otel`) | OTel/OpenInference bridge |
| `moa-lineage/citation` (`moa-lineage-citation`) | Citation/provenance adapters |
| `moa-lineage/cold` (`moa-lineage-cold`) | Cold lineage export and partition support |
| `moa-lineage/audit` (`moa-lineage-audit`) | Compliance audit hashes, Merkle roots, signing, DSAR support |
| `moa-authz-schema` | Planned, Phase 1: typed FGA tuple keys and schema v1 constants |
| `moa-fga-bootstrap` | Planned, Phase 1: idempotent OpenFGA store and model bootstrap binary |
| `moa-authz` | Planned, Phase 1: FGA client, transactional outbox, and outbox poller |
| `moa-edge` | Planned, Phase 1: public HTTP edge for token validation and identity header injection |
| `moa-auth-providers-auth0` | Planned, Phase 1: optional Auth0 implementations gated by the `auth0` Cargo feature |
| `moa-ocsf` | Planned, Phase 1: OCSF v1.3 security-event types, emission helpers, and signing |
| `moa-hands` | Tool routing and hand providers |
| `moa-providers` | LLM and embedding providers |
| `moa-orchestrator` | Restate handlers and cloud orchestration binary |
| `moa-gateway` | Messaging adapters and renderers |
| `moa-runtime` | Shared runtime assembly |
| `moa-cli` | Thin-client CLI and orchestrator diagnostics |
| `moa-security` | Vault, policies, MCP credential proxy, injection controls |
| `moa-skills` | Skill parsing, distillation, improvement, regression generation |
| `moa-eval` | Evaluation harness |
| `moa-loadtest` | Load-test tooling |
| `workspace-hack` | Generated `cargo-hakari` dependency feature unification crate |
| `xtask` | Repo-local audit and maintenance commands |

## Where To Look Next

- Orchestration details: `docs/02-brain-orchestration.md` and `docs/12-restate-architecture.md`
- Memory details: `docs/04-memory-architecture.md`
- Shared type placement: `docs/architecture/type-placement.md`
- Event and segment schema: `docs/05-session-event-log.md`
- Context pipeline: `docs/07-context-pipeline.md`
- Multi-tenant learning: `docs/14-multi-tenancy-and-learning.md`
