# 01 — Architecture Overview

_System model, trait map, data flow, and workspace layout._

## System Model

```text
Clients
  REST/gateway | API automation | Slack
        |
        v
Runtime boundary
  Edge HTTP reads/writes plus `moa-orchestrator` Restate handler service
        |
        v
Brain and execution
  Context pipeline -> provider router -> LLM
  Tool router -> built-ins / hands / MCP
  Worker dispatch -> Restate Worker virtual objects
        |
        v
Product data in Postgres / Neon
  sessions, events, pending_signals, context_snapshots
  task_segments, experience_records, experience_attributions,
  learning_candidates, segment and strategy materialized views
  graph nodes, graph edges, sidecar indexes, configured vector records
  knowledge connections, sync runs, document versions, chunks
  learning_log
  analytics.turn_lineage, analytics.score_run, analytics.scores,
  moa.experiment_run, compliance audit tables
  security_events
        |
        v
Learning loop
  segments -> segment assessments -> experience records
  experience records -> attributions -> learning candidates
  task-conditioned strategy rates -> skill ranking
  learning log -> skill ranking, memory consolidation, rollback audit
```

Restate owns durable cloud execution. Postgres owns product-visible data and
cross-pod correctness records. The optional runtime cache is selected through
typed config: Redis coordinates short-lived runtime pacing or references across
replicas when configured, while the in-memory backend is process-local and must
not be used as an authoritative Kubernetes store. Graph memory is the canonical
memory source, with sidecar and vector indexes maintained by graph writes.
Tenant knowledge-base ingestion is a separate product surface owned by
`moa-knowledge`; it writes tenant knowledge into the same graph/vector substrate
without turning connector sync into session memory ingestion.

## Agent Building Blocks

MOA has one user-facing capability artifact kind: the skill. A skill is an
open-ended, agent-mediated capability selected by the context pipeline and
executed through the existing `Session` and `TurnExecution` path. Skills give an
agent instructions, tools, memory, approvals, and workers so it can handle a
task autonomously.

A skill may additionally declare an optional deterministic `procedure` in its
`skill.moa.yaml` definition. A procedure is a graph-shaped execution plan
(`ProcedureDefinition`) used when conditions, approval gates, connector actions,
checkpoints, memory operations, bounded agent/worker adapter nodes, and run
history must be explicit and reviewable. It is the deterministic execution mode
of the same skill artifact, not a separate artifact shape; skills without a
procedure keep the open-ended agent-mediated behavior.

Agents, skills, connectors, actions, and behavior-lab experiment plans are
canonical artifacts. `moa-artifacts` owns the persisted document model,
validation, stable references, revision history, and Postgres registry;
`moa-skills` owns skill package parsing, draft proposal generation,
artifact-backed package helpers, and the pure deterministic procedure
interpreter (`ProcedureInterpreter`) with its graph-renderable execution state;
`moa-orchestrator` owns Restate execution through `ProcedureExecution` and
adapter calls into existing services. JSON is the canonical persisted shape in
Postgres, while YAML is a human authoring/import/export format. Visual builders
must round-trip through the same artifact structs instead of owning a separate
canvas-only model; optional `ui` metadata is non-semantic layout/canvas data.

Behavior Lab uses a single `experiment_plan` artifact. Personas, profiles, data bundles, and scenarios are typed embedded blocks under `definition.spec.simulation`, each with stable IDs for UI round trips, trial fanout, scoring, and analytics. Their product boundary, UI expectations, and verification lanes are documented in [`docs/product/behavior-lab.md`](product/behavior-lab.md).

Every durable session is created inside one tenant for a contact, or by an
admin/operator actor, and uses a pinned agent revision. The session row owns the
tenant, contact, and creator attribution, while the `session_agent_context`
sidecar stores the selected agent artifact revision, deployment pointers when
present, policy hash, locked artifact/tool dependencies, and serialized runtime
policy snapshot. Per-agent guardrail policy is stored in the DB-backed agent
artifact JSON and pinned into this `session_agent_context` snapshot as
`guardrail_policy`. The context pipeline still ranks visible published skill
artifact revisions and materializes selected artifact files for the tool
router, but that selection now runs inside the configured agent policy for the
session.
Skill procedures are explicit product operations run through the Skills surface
(`/v1/skills/runs/list`, `/v1/skills/run`, `/v1/skills/status`,
`/v1/skills/cancel`, `/v1/skills/signal`, `/v1/skills/decide-review`); a run
may be associated with a session for UI/history. Starting a procedure validates
caller inputs against the skill's input schema and returns a structured
missing-inputs error instead of creating a run when required inputs are absent.
The procedure runtime interprets explicit graph nodes; the open-ended agent loop
does not implicitly choose procedure graphs. An agent can invoke a selected
skill's procedure through a policy-gated hands tool, which enforces the same
input-schema check before a run starts.

Current artifact tables are `moa.artifact`, `moa.artifact_revision`, `moa.artifact_file`, `moa.artifact_run`, and `moa.artifact_node_run`. `moa.artifact` / `moa.artifact_revision` are the source of truth for skill packages, and `moa.artifact_run` / `moa.artifact_node_run` persist procedure runs and their per-node execution state. Automatic skill learning follows `skill proposal -> draft skill artifact + learning_candidate -> LearningReview accept -> published artifact`; generation never rewrites published skill revisions directly.

Tenant knowledge-base rows are `moa.knowledge_connections`,
`moa.knowledge_sync_runs`, `moa.knowledge_ingestion_steps`,
`moa.knowledge_objects`, `moa.knowledge_document_versions`,
`moa.knowledge_blocks`, and `moa.knowledge_chunks`. These rows describe linked
external accounts, sync-run inspection state, parser output, block/chunk
identity, and graph write status. They are not session events and are not
written through `Memory.ingest_documents`.

MOA's runtime boundary is the tenant. Runtime operators can run local mode for
development and incident response, but the product model assumes organizations
need governed execution, audit trails, tenant-owned learning, and clear rollback
paths.

## Tenant Model

```text
workspace
  -> tenant
       -> contact
            -> session
```

This is the complete runtime hierarchy. The workspace is the deployment
administration boundary. Workspace admins are super-admin principals whose
OpenFGA `workspace#admin` relation inherits into every linked tenant's
`tenant#admin` relation. A tenant remains the hard runtime isolation boundary:
sessions, contacts, memory, learning, artifacts, analytics, policies, events,
and audit evidence are tenant-owned.

Contacts are end users inside a tenant. Operators are admin/control-plane
principals: workspace admins, tenant admins, tenant operators, service users,
and API-key subjects. Operators are authorized to administer or operate tenants,
but they are not contact memory subjects and are not part of the contact/session lineage.
Contact credentials cannot access platform-internal control-plane surfaces such
as skills, experiments, knowledge management, or tenant administration.

Contact memory is contact-local. A contact session never inherits another
contact's memory or tenant admin/operator memory. When graph memory is enabled,
the default answer-time retrieval path combines tenant knowledge-base chunks
with admitted memory for the current contact, then keeps those source tiers
separate in prompt context and query trace records. Tenant learning is
tenant-local and never globally promoted. Skills and policies are tenant-owned.

## Core Traits

Most trait definitions live under `crates/moa-core/src/traits/` and
`crates/moa-core/src/traits/mod.rs`; shared DTOs live under
`crates/moa-core/src/types/`. A few traits are owned by the crate that
implements them: `Reranker` in `moa-providers`, and `LinkedIntegrationProvider`
and `DocumentParser` in `moa-knowledge`. Session orchestration is not a trait —
it is realized as Restate services and virtual objects in `moa-orchestrator`
(see `docs/12-restate-architecture.md`).

| Trait | Purpose | Main implementations |
|---|---|---|
| `SessionStore` | Append-only event log, sessions, pending signals, snapshots, task segments, experience records, learning candidates, analytics, skill rates | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `PostgresBlobStore` by default; explicit `FileBlobStore` for local or mounted-path use |
| `RuntimeCacheStore` | Short-lived runtime coordination/cache values with TTL | in-process memory fallback; optional Redis backend |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `HandProvider` | Provision, execute, pause/resume, destroy hands | local, Docker, Daytona, E2B |
| `LLMProvider` | Provider completion interface | Anthropic, OpenAI, Gemini through `moa-providers` |
| `EmbeddingProvider` | Shared embedding interface | OpenAI embedding, Cohere v4, ZeroEntropy zembed-1, Gemini embedding, and test/mock adapters |
| `Reranker` | Shared reranking interface | Noop, Cohere Rerank, and ZeroEntropy rerank through `moa-providers` |
| `ChannelAdapter` | Channel inbound/outbound normalization | Slack |
| `BuiltInTool` | Built-in tool execution | memory/search/web and other built-ins |
| `ContextProcessor` | One stage in context compilation | identity, agent instructions, instructions, tools, query rewrite, skills, digest, memory, history, delegation planning, runtime context |
| `LinkedIntegrationProvider` | Tenant knowledge linked-account flow, provider sync trigger, changed-record listing, and webhook verification | Nango and Merge adapters in `moa-knowledge` |
| `DocumentParser` | Structure-aware parsing into normalized document elements for tenant knowledge ingestion | Native parser backed by `liteparse` for local file parsing, plus LlamaParse, Unstructured, and Reducto adapters in `moa-knowledge` |
| `CredentialVault` | Secret storage and retrieval | environment-backed MCP vault |
| `LineageHandle` | Transport-neutral lineage capture | null handle, async sink, OTel bridge |

Runtime entrypoints share these seams through the Restate-backed orchestrator.
Phase 1 auth work adds `AuthProvider`, `TokenVaultProvider`, and
`AsyncAuthzProvider` to `moa-core::traits`; see ADR-0002.

## Runtime Modes

### Cloud

`moa-orchestrator` exposes Restate handlers from one production binary. Domain
logic behind those handlers should live in in-process application services,
repositories, or domain crates so a future extraction can replace a composition
binding without changing handler contracts.

Core production bindings:

- Virtual objects: `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO`
- Services: `ActionReviews`, `AgentDefinitions`, `Agents`,
  `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`,
  `Contacts`, `Eval`, `Experiments`, `GraphMemoryMaint`, `Knowledge`,
  `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`,
  `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy`
- Workflows: `ProcedureExecution`, `KnowledgeSyncIngestion`,
  `Consolidate`, `ExperimentRun`, `ExperimentTrialRun`, `TurnExecution`,
  `WorkerTurnExecution`

Feature-gated bindings:


Internal application boundaries are in-process modules or domain crates behind
these handlers, not separate network services. Current examples include action
review policy and storage, builtin async-authz challenge storage, learning
review promotion, experiments, privacy, provider routing, and graph memory
retrieval. Read-only analytics, identity diagnostics, audit verification, and
lineage read routes are served directly from `moa-edge` against Postgres/domain
stores instead of going through Restate.

`Session` is the durable actor for one session key. It queues messages, admits `TurnExecution` workflows, tracks the active task segment, records tool/skill usage, and writes learning entries. Segment assessment happens at turn, segment, idle, cancellation, and timeout boundaries as an auditable learning artifact, not as a live-loop control signal. `Worker` owns conversational delegated state with depth and budget limits, while `WorkerTurnExecution` runs one admitted child turn and reports turn-scoped mutations back to the VO.

Coordinator turns can return while detached workers keep running across
non-sticky replicas. The coordinator and its children coordinate over two planes —
a high-frequency telemetry plane (progress, heartbeat, one-call-per-period
narration) that stays off the single-writer `Session` VO, and a low-frequency
control plane (attention signals, terminal-wake, guarded resume) that routes
through the coordinator VO. All of this is correct on Kubernetes because its state
lives in Restate VO/workflow state and Postgres (idempotent event appends guarded
by `session_event_dedupe`); Redis is a runtime cache only and never a correctness
owner. The root coordinator is sandbox-free, and each worker owns one ephemeral
sandbox keyed `(session_id, worker_id, provider)` that is released on the
worker's self-cleanup. `docs/02-brain-orchestration.md` and
`docs/12-restate-architecture.md` describe these planes in detail.

### Hosted API Clients

MOA ships no embedded command/runtime client. Local development and automation
exercise the same hosted surface as production: callers send HTTP requests to
`moa-edge` public routes or directly to Restate ingress in test fixtures.
Client code does not own sessions, memory, sandbox lifecycle, tool execution,
approvals, or code execution.

## Turn Data Flow

```text
User message
  -> Input guardrail evaluates configured-agent text policy
  -> SessionStore emits `UserMessage`
  -> Session VO prepares a turn
  -> Context pipeline runs
       1 identity
       2 instructions
       3 tools
       4 query_rewrite (when enabled)
       5 skills
       6 memory_digest (when enabled)
       7 memory
       8 history
       8 delegation_planning
       9 runtime_context
  -> Query rewrite may mark `is_new_task`
  -> SegmentTracker opens or rolls a task segment
  -> LLM response is streamed/collected
  -> Tool calls route through ToolExecutor and ToolRouter
  -> Output guardrail evaluates buffered visible text
  -> BrainResponse and tool events are persisted
  -> Segment counters are updated
  -> SegmentAssessor assesses completed or idle segments
  -> Assessed segments emit experience records and attributions
  -> Learning candidates propose skill, memory, policy, prompt, or eval updates
  -> LearningEntry rows record promoted segment, skill, or memory learning
```

If query rewriting is disabled, the `query_rewrite` processor is omitted and
the remaining processors still report their configured stage numbers.

V1 guardrails are optional configured-agent LLM-judge text policies. Input
guardrails run before a `UserMessage` event is appended; output guardrails run
after the main model response text is buffered and before the visible
`BrainResponse` event is appended. `GuardrailCheck` events record decision
metadata for audit without storing the raw guarded text. PII guardrails,
response-schema guardrails, and tool input/output guardrails are explicitly out
of scope for V1, and guardrails are not a replacement for action or tool
policy.

## Storage Overview

| Area | Store | Notes |
|---|---|---|
| Session metadata and events | Postgres | `sessions`, `events`, `pending_signals`, `context_snapshots` |
| Task segmentation | Postgres | `task_segments`, segment baselines, skill resolution rates |
| Experience learning | Postgres | `experience_records`, `experience_attributions`, `learning_candidates`, task-conditioned strategy rates |
| Live behavior experiments | Postgres | `moa.experiment_run`, `moa.experiment_run_artifact_revision`, and linked `analytics.score_run` rows |
| Tenant knowledge base | Postgres | `moa-knowledge` owns linked connections, sync runs, ingestion steps, document versions, blocks, chunks, and provider event state; `moa-memory-*` owns the resulting graph/vector storage |
| Graph memory | Postgres | Nodes, edges, sidecar indexes, changelog, and RLS-protected scope state |
| Memory vectors | Postgres or configured vector backend | pgvector embeddings or Turbopuffer namespaces for graph retrieval; graph storage remains relational Postgres |
| Skill packages | Postgres | `moa.artifact`, `moa.artifact_revision`, and `moa.artifact_file` store tenant-owned skill documents and package bytes; generated tenant updates first land as tenant-scoped draft skill artifacts plus proposed `learning_candidates` and only become active after review acceptance |
| Learning audit | Postgres | `learning_log` append-only rows with bitemporal validity |
| Hand leases | Postgres | `moa.hand_leases` stores session/provider sandbox bindings, serialized handles, generation fencing, status, and expiry for cross-pod reuse and cleanup |
| Claim-check blobs | Postgres by default | large event payloads use `session_blobs`; local filesystem blobs require explicit configuration and a persistent mounted path in cloud |
| Session attachments | Postgres + object storage | `session_attachments` stores metadata and object keys; bytes live in RustFS locally or AWS S3/GCS in cloud; session events carry `Attachment` refs with durable ids |
| Cloud orchestration state | Restate | VO/workflow state and journals, not product record |
| Runtime cache | Redis or memory | optional TTL cache/coordination for pacing and transient references; memory is per-process and non-authoritative |
| Optional checkpoints | Neon | branch manager for database checkpoints |
| Security events | Postgres and S3 | OCSF v1.3 events in `security_events`, shipped to tenant audit buckets |

## Auth Layer

ADR-0002 is the canonical decision record for MOA's identity,
authorization, naming, and security-event audit posture.
The implementation crates are grouped under `crates/moa-auth/` as a namespace
folder while preserving package names and Rust imports.

### Identity

`moa-edge` validates API keys by default, or OIDC tokens in Auth0/OIDC modes.
For local development and isolated tests, `auth.provider = "disabled"` accepts
unauthenticated edge requests as a fixed service identity. After authentication,
the edge injects `X-Moa-Identity-Type`, `X-Moa-Identity-Id`,
`X-Moa-Tenant-Id`, `X-Moa-Acting-On-Behalf-Of`, and
`X-Moa-Api-Key-Id` headers before forwarding to the orchestrator. The
orchestrator trusts these headers, so the Restate handler port (`9080`) must be
network-isolated in production; see
[`docs/operations/edge-network-isolation.md`](operations/edge-network-isolation.md).

Local deployments use the zero-dependency provider bundle by default:
`LocalAuthProvider`, `NullTokenVaultProvider`, and
`BuiltinAsyncAuthzProvider`. Builtin approvals are documented in
[`docs/operations/builtin-approvals.md`](operations/builtin-approvals.md).
Auth0 setup is documented in
[`docs/operations/auth0-setup.md`](operations/auth0-setup.md).
Agent lifecycle operations are documented in
[`docs/operations/agent-lifecycle.md`](operations/agent-lifecycle.md), and the
Auth0 Token Vault setup is documented in
[`docs/operations/token-vault-setup.md`](operations/token-vault-setup.md).
SCIM v2 provisioning is documented in
[`docs/auth/scim.md`](auth/scim.md). OCSF security-event audit setup is
documented in [`docs/operations/ocsf-audit.md`](operations/ocsf-audit.md).

### Caller Identity Vs Contact

MOA identity is the authenticated control-plane caller: tenant admins, tenant
operators, API keys, service users, and future SSO/OIDC users. These
admin/operator principals are authorized through OpenFGA before protected reads
or writes. Runtime access to tenant-owned data uses tenant relations.

Contacts are agent-facing people or anonymous browser/device handles that
interact with an agent inside one tenant. Contacts are not admin/operator users
and are not provisioned through SCIM. A tenant admin, tenant operator, or
authorized integration issues a bounded MOA contact JWT; unverified contacts can
create/send agent-session messages with low-assurance scopes, and verification
workflows can promote the session to a verified contact. Sessions, memory,
analytics, traces, and privacy workflows carry the tenant id, session id, and
contact id for observability.

### Authorization

OpenFGA, backed by Postgres, is the default authorization engine. The v1
authorization schema lives in `moa-authz-schema` as Rust constants. Handlers
call `require_authz(authz, identity, object_type, object_id, relation)` at the
behavior boundary; there are no procedural macros or implicit handler guards.

### Audit

OCSF v1.3 security events are written synchronously to a Postgres
`security_events` table. The existing `services/audit-shipper` service ships
those events to S3 with per-tenant bucket routing and Object Lock compliance
mode. Operational setup is documented in
[`docs/operations/ocsf-audit.md`](operations/ocsf-audit.md).

## Eval, Experiments, And Insights

Lineage records are captured through the hot-path `LineageHandle` bridge and
written asynchronously to `analytics.turn_lineage`. Eval, online-judge, and
human-review scores use the same sink via `LineageEvent::Eval(ScoreRecord)` and
land in `analytics.scores`, keyed by turn, session, or dataset replay item.

Regression evals, live behavior experiments, and analytics insights are
separate surfaces:

- Regression eval: `moa-eval` owns deterministic datasets, replay plans,
  CI/nightly regression runs, and score comparisons. In default cloud builds
  the public edge does not translate `/v1/evals/*`. The `Eval` service is
  compiled into the orchestrator as an internal control-plane surface; hosted
  run status is stored in `analytics.eval_run_status` so it is not a Restate
  workflow-state mirror. Tenant-owned plan, run, replay, dataset, score, and
  compare handlers require the tenant operator relation, which includes tenant
  admins in the OpenFGA model. The detached `Eval/execute_run` worker entrypoint
  is not caller-authorized directly; it must carry the dispatch token created
  by the authorized `Eval/run` admission path before it can return or mutate run
  data.
- Live behavior experiments: `moa-experiments` owns the typed domain model and
  storage repository; the `Experiments` service accepts and tracks runs against
  production execution paths. Agent-loop targets create or reuse `Session`
  state and queue messages through the normal `Session` and `TurnExecution`
  path. Procedure targets start skill procedure runs through the procedure
  runtime, link `moa.artifact_run.run_uid`, and execute supported deterministic
  procedure nodes through `ProcedureExecution`. The `moa.experiment_run` row is
  the experiment ledger and links to the session, procedure run, pinned artifact
  revisions, and `analytics.score_run`.
  `ExperimentTrialRun` owns per-trial simulator execution. The public edge
  routes are `POST /v1/experiments/generate-plan`,
  `/v1/experiments/run-plan`, `/v1/experiments/status`,
  `/v1/experiments/list`, `/v1/experiments/plans/list`,
  `/v1/experiments/trials`, `/v1/experiments/trial-status`,
  `/v1/experiments/cancel`, `/v1/experiments/propose-improvements`,
  `/v1/experiments/scores`, and `/v1/experiments/compare`; stale aliases such
  as `/v1/experiments/run` are
  not product routes. `Experiments/generate_plan`, `run`, `cancel`, and
  `propose_improvements` require a tenant admin or tenant operator relation;
  `status`, `list`, `trials`, `trial_status`, `scores`, and `compare` require
  tenant participation, tenant operator, or tenant admin authorization according
  to the target resource.
- Analytics and insights: `moa-edge` exposes `GET /v1/analytics/catalog` and
  `POST /v1/analytics/query` for tenant operator dashboards backed by curated
  analytics read models. The handlers read Postgres/domain stores directly after
  authz instead of paying a Restate hop for single-query reads. Future analytics
  agents must call these typed routes or application services, not raw SQL or
  unscoped `SessionStore` methods. Analytics catalog and query reads require
  tenant admin or tenant operator authorization and always scope rows to the
  explicitly requested tenant when supplied, otherwise to the authenticated
  identity's tenant. Audit verification and lineage explain/query/verify
  remain direct read use cases with their own typed handlers.
  Lineage export and erase are intentionally not direct read handlers until a
  durable product workflow owns those side effects.

Live behavior experiment-derived improvements have one review boundary: they
must become `learning_candidates` before any skill change is
accepted. Experiment runs do not auto-create those proposals, and no experiment
path may auto-promote skills. The explicit
`Experiments/propose_improvements` operation attaches experiment run IDs, score
run IDs, and artifact revision references to the candidate payload so reviewers
can reproduce the evidence.

Skill-derived improvements use the same boundary. `TurnExecution` may dispatch a
detached `SkillLearning` workflow after experience persistence, but that
workflow can only create tenant-scoped draft skill artifacts and proposed
`LearningCandidateType::Skill` rows. Tenant-learned skills remain tenant-local
and are never promoted into shared defaults automatically. `LearningReview`
is the only runtime path that publishes those drafts inside the tenant, records
`skill_created`/`skill_improved`, and marks the candidate promoted.

Future MCP support is a transport adapter over product/default services such as
`Experiments`, direct edge analytics/lineage reads, `Skills`, and other typed
surfaces. If internal eval is exposed through MCP, it must remain explicitly
internal and operator/admin-authorized. MCP must not publish public
`/v1/evals/*` semantics, own experiment, eval, analytics, learning, or lineage
domain models, or bypass service-level authorization.

Grafana dashboards live in `dashboards/grafana/` and Prometheus alert rules live
in `ops/prometheus/alerts/`. Import the dashboards with a Postgres datasource
named `DS_POSTGRES` and a Prometheus datasource named `DS_PROMETHEUS`; the
tenant selector is populated from `analytics.turn_lineage`.

## Compliance Audit Tier

Compliance audit is an opt-in superset of the engineering lineage tier. A
control-plane enrollment row enables tenant-local BLAKE3 chain links on
`analytics.turn_lineage`, periodic Merkle roots in `analytics.audit_roots`, PII
pseudonymization side data in `pii_vault`, and hosted verification through
`POST /v1/lineage/verify`. Export and erase side effects stay out of direct edge
read routes until a durable DSAR workflow owns them. Tenants that are not
enabled keep the L01-L03 behavior and store `prev_hash = NULL`.

Audit bucket bootstrap lives in `scripts/bootstrap-audit-bucket.sh`. Buckets
must be created with Object Lock enabled at creation time; production uses
Compliance mode and development uses a separate bucket, usually Governance mode
with short retention. Audit-root signing always uses the in-process Ed25519
signer configured by `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX`; production must
provision that key through the runtime secret manager. Switching signing keys
starts new windows with the new label; old verifying keys remain required for
old audit roots. The hosted lineage root verifier is fail-closed: audit-root
window verification requires `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX` and a matching
`MOA_LINEAGE_AUDIT_SIGNING_KEY_ID` for the root's stored signing-key label.
Lineage DSAR bundle export uses the privacy export signing key contract,
`MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX` plus optional
`MOA_PRIVACY_EXPORT_SIGNING_KEY_ID`.

**ATTESTATION GATE - DO NOT REPRESENT THIS AS COMPLIANCE EVIDENCE TO REGULATORS
OR CUSTOMERS UNTIL EXTERNAL CRYPTOGRAPHIC REVIEW IS COMPLETE.**
`moa-lineage-audit` must receive external cryptographer or appsec review before
DSAR exports, regulator responses, audit attestations, or certifications rely on
this layer as compliance-grade evidence. Internal debugging and forensics may
use it before that review. The review must cover BLAKE3 canonicalization and
chain extension, Ed25519 key handling, Merkle inclusion and consistency proof
construction, PII crypto-shredding semantics, S3 Object Lock configuration,
timestamp discipline, and replay resistance on the verify path.

## Workspace Layout

The complete package inventory changes often enough that duplicating it here
creates drift. Use the root [`README.md`](../README.md#workspace-layout) for a
scan-friendly crate map, and [`docs/10-technology-stack.md`](10-technology-stack.md)
for the current full workspace list from `cargo metadata`.

## Where To Look Next

- Orchestration details: `docs/02-brain-orchestration.md` and `docs/12-restate-architecture.md`
- Memory details: `docs/04-memory-architecture.md`
- Shared type placement: `docs/15-architecture-policy.md`
- Event and segment schema: `docs/05-session-event-log.md`
- Live behavior experiments: `docs/eval/live-behavior-experiments.md`
- Context pipeline: `docs/07-context-pipeline.md`
- Multi-tenant learning: `docs/14-multi-tenancy-and-learning.md`
