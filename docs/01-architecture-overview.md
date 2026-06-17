# 01 — Architecture Overview

_System model, trait map, data flow, and workspace layout._

## System Model

```text
Clients
  REST/gateway | API automation | Slack
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
  task_segments, experience_records, experience_attributions,
  learning_candidates, segment and strategy materialized views
  graph nodes, graph edges, sidecar indexes, pgvector embeddings
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

Restate owns durable cloud execution. Postgres owns product-visible data. Graph memory is the canonical memory source, with sidecar and vector indexes maintained by graph writes.

## Agent Building Blocks

MOA supports two user-facing execution shapes:

- Agent loop: the existing `Session` and `TurnExecution` path gives an agent tools, skills, memory, approvals, and sub-agents so it can handle an open-ended task autonomously.
- Agent workflow: an artifact-backed `WorkflowDefinition` stores a typed node/edge graph for cases that need explicit conditions, approval gates, connector actions, checkpoints, and run history.

Skills, connectors, actions, workflows, and behavior-lab experiment plans are canonical artifacts. `moa-artifacts` owns the persisted document model, validation, stable references, and Postgres registry; `moa-skills` owns skill parsing, ranking, distillation, and the live `SkillInjector` path; `moa-workflows` owns durable workflow run lifecycle and the future node interpreter/improvement loop. JSON is the canonical persisted shape in Postgres, while YAML is a human authoring/import/export format. Visual builders must round-trip through the same artifact structs instead of owning a separate canvas-only model; optional `ui` metadata is non-semantic layout/canvas data.

Behavior Lab uses a single `experiment_plan` artifact. Personas, profiles, data bundles, and scenarios are typed embedded blocks under `definition.spec.simulation`, each with stable IDs for UI round trips, trial fanout, scoring, and analytics. Their product boundary, UI expectations, and verification lanes are documented in [`docs/product/behavior-lab.md`](product/behavior-lab.md).

At runtime today, agent-loop choice is skill-driven: the context pipeline ranks visible skills and materializes selected skill files for the tool router. Artifact-backed workflows are explicit product operations through the `Workflows` API; a run may be associated with a session for UI/history, but the open-ended agent loop does not yet select or interpret workflow nodes automatically.

Current artifact tables are `moa.artifact`, `moa.artifact_revision`, `moa.artifact_file`, `moa.artifact_run`, and `moa.artifact_node_run`. Existing `moa.skill` and `moa.skill_file` rows remain the materialized skill lookup used by `SkillInjector`.

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
       -> Tenant learning log
       -> Workspace memory
       -> Global, workspace, and user skills ranked by tenant-level outcomes
       -> Lineage, analytics, and optional compliance evidence
```

Learning entries and outcome aggregates are tenant-scoped because teams tend to repeat work patterns across projects. Skill packages use the same three-tier scope model as durable memory: global skills are visible to every workspace, workspace skills are visible inside one workspace, and user skills are visible only to that workspace-bound user. Ranking signals aggregate at tenant level.

## Core Traits

Current trait definitions live under `crates/moa-core/src/traits/` and
`crates/moa-core/src/traits/mod.rs`; shared DTOs live under
`crates/moa-core/src/types/`.

| Trait | Purpose | Main implementations |
|---|---|---|
| `BrainOrchestrator` | Start, resume, signal, list, observe sessions; schedule background work | Restate services/objects through `moa-orchestrator` |
| `SessionStore` | Append-only event log, sessions, pending signals, snapshots, task segments, experience records, learning candidates, analytics, skill rates | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `FileBlobStore` |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `HandProvider` | Provision, execute, pause/resume, destroy hands | local, Docker, Daytona, E2B |
| `LLMProvider` | Provider completion interface | Anthropic, OpenAI, Gemini through `moa-providers` |
| `EmbeddingProvider` | Shared embedding interface | OpenAI embedding, Cohere v4, Gemini embedding, and test/mock adapters |
| `PlatformAdapter` | Gateway inbound/outbound normalization | Slack |
| `BuiltInTool` | Built-in tool execution | memory/search/web and other built-ins |
| `ContextProcessor` | One stage in context compilation | identity, instructions, tools, query rewrite, skills, memory, history, runtime context, compactor |
| `CredentialVault` | Secret storage and retrieval | local encrypted vault; environment-backed MCP vault |
| `LineageHandle` | Transport-neutral lineage capture | null handle, async sink, OTel bridge |

Runtime entrypoints share these seams through the Restate-backed orchestrator.
Phase 1 auth work adds `AuthProvider`, `TokenVaultProvider`, and
`AsyncAuthzProvider` to `moa-core::traits`; see ADR-0002.

## Runtime Modes

### Cloud

`moa-orchestrator` exposes Restate handlers:

- Virtual objects: `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO`
- Services: `Agents`, `AdminMaintenance`, `Analytics`, `Approvals`, `ApiKeys`, `Artifacts`, `Audit`, `Authz`,
  `Experiments`, `GraphMemoryMaint`, `Health`, `LineageAdmin`, `LLMGateway`,
  `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`,
  `ToolExecutor`, `Workflows`, `WorkspaceStore`, `Whoami`
- Workflows: `Consolidate`, `ExperimentRun`, `ExperimentTrialRun`,
  `TurnExecution`, `SubAgentTurnExecution`

When built with `internal-eval-runner`, the orchestrator also binds internal
`Eval` and `EvalRun` handlers. They are not part of the default cloud
registration.

`Session` is the durable actor for one session key. It queues messages, admits `TurnExecution` workflows, tracks the active task segment, records tool/skill usage, and writes learning entries. Segment assessment happens at turn, segment, idle, cancellation, and timeout boundaries as an auditable learning artifact, not as a live-loop control signal. `SubAgent` owns conversational delegated state with depth and budget limits, while `SubAgentTurnExecution` runs one admitted child turn and reports turn-scoped mutations back to the VO.

### Hosted API Clients

MOA ships no embedded command/runtime client. Local development and automation
exercise the same hosted surface as production: callers send HTTP requests to
`moa-edge` public routes or directly to Restate ingress in test fixtures.
Client code does not own sessions, memory, sandbox lifecycle, tool execution,
approvals, or code execution.

## Turn Data Flow

```text
User message
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
       9 runtime_context
       10 compactor
  -> Query rewrite may mark `is_new_task`
  -> SegmentTracker opens or rolls a task segment
  -> LLM response is streamed/collected
  -> Tool calls route through ToolExecutor and ToolRouter
  -> BrainResponse and tool events are persisted
  -> Segment counters are updated
  -> SegmentAssessor assesses completed or idle segments
  -> Assessed segments emit experience records and attributions
  -> Learning candidates propose skill, memory, policy, prompt, or eval updates
  -> LearningEntry rows record promoted segment, skill, or memory learning
```

If query rewriting is disabled, stage 5 is omitted and the remaining processors still report their configured stage numbers.

## Storage Overview

| Area | Store | Notes |
|---|---|---|
| Session metadata and events | Postgres | `sessions`, `events`, `pending_signals`, `context_snapshots` |
| Task segmentation | Postgres | `task_segments`, segment baselines, skill resolution rates |
| Experience learning | Postgres | `experience_records`, `experience_attributions`, `learning_candidates`, task-conditioned strategy rates |
| Live behavior experiments | Postgres | `moa.experiment_run`, `moa.experiment_run_artifact_revision`, and linked `analytics.score_run` rows |
| Graph memory | Postgres | Nodes, edges, sidecar indexes, changelog, and RLS-protected scope state |
| Memory vectors | Postgres | pgvector embeddings for graph retrieval |
| Skill packages | Postgres | `moa.skill` metadata and `moa.skill_file` package bytes for global, workspace, and user scopes; selected packages are materialized into hands on demand |
| Learning audit | Postgres | `learning_log` append-only rows with bitemporal validity |
| Cloud orchestration state | Restate | VO/workflow state and journals, not product record |
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
[`docs/operations/auth0-setup.md`](operations/auth0-setup.md), and optional
OIDC group-to-FGA tuple naming is documented in
[`docs/operations/oidc-group-mapping.md`](operations/oidc-group-mapping.md).
Agent lifecycle operations are documented in
[`docs/operations/agent-lifecycle.md`](operations/agent-lifecycle.md), and the
Auth0 Token Vault setup is documented in
[`docs/operations/token-vault-setup.md`](operations/token-vault-setup.md).
SCIM v2 provisioning is documented in
[`docs/auth/scim.md`](auth/scim.md). OCSF security-event audit setup is
documented in [`docs/operations/ocsf-audit.md`](operations/ocsf-audit.md).

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
  these remain local and CI tooling; the public edge does not translate
  `/v1/evals/*`. The `Eval` service and `EvalRun` workflow are available
  only when the orchestrator is compiled with `internal-eval-runner`; if
  exposed internally, their handlers still enforce workspace authorization
  before replay, dataset, score, or compare reads.
- Live behavior experiments: `moa-experiments` owns the typed domain model and
  storage repository; the `Experiments` service accepts and tracks runs against
  production execution paths. Agent-loop targets create or reuse `Session`
  state and queue messages through the normal `Session` and `TurnExecution`
  path. Workflow targets start existing artifact-backed runs through
  `WorkflowRuntime` and link `moa.artifact_run.run_uid`; workflow node
  interpretation remains a future `moa-workflows` capability. The
  `moa.experiment_run` row is the experiment ledger and links to the session,
  workflow run, pinned artifact revisions, and `analytics.score_run`.
  `ExperimentTrialRun` owns per-trial simulator execution. The public edge
  routes are `POST /v1/experiments/generate-plan`,
  `/v1/experiments/run-plan`, `/v1/experiments/status`,
  `/v1/experiments/list`, `/v1/experiments/trials`,
  `/v1/experiments/trial-status`, `/v1/experiments/cancel`,
  `/v1/experiments/propose-improvements`, `/v1/experiments/scores`, and
  `/v1/experiments/compare`; stale aliases such as `/v1/experiments/run` are
  not product routes. `Experiments/generate_plan`, `run`, `cancel`, and
  `propose_improvements` require `Workspace:Editor`; `status`, `list`,
  `trials`, `trial_status`, `scores`, and `compare` require
  `Workspace:Member`.
- Analytics and insights: `Analytics` exposes curated read APIs for session,
  workspace, tool, cache, experiment, learning-candidate, and session-search
  use cases. `LineageAdmin` exposes protected lineage explain, query, export,
  verify, and erase operations. Future analytics agents must call these typed
  services, not raw SQL or unscoped `SessionStore` methods. `Analytics`
  session stats require `Session:Participant`; workspace, cache, experiment,
  and session-search reads require `Workspace:Member`; workspace learning
  candidate reads require `Workspace:Editor`; tenant-wide learning candidates
  and deployment-wide tool stats require tenant-admin authorization, with
  deployment-wide tool stats limited to service identities. `LineageAdmin`
  explain, query, and verify require `Workspace:Member`, while export and erase
  require `Workspace:Admin`.

Live behavior experiment-derived improvements have one review boundary: they
must become `learning_candidates` before any skill or workflow change is
accepted. Experiment runs do not auto-create those proposals, and no experiment
path may auto-promote skills or workflows. The explicit
`Experiments/propose_improvements` operation attaches experiment run IDs, score
run IDs, and artifact revision references to the candidate payload so reviewers
can reproduce the evidence.

Future MCP support is a transport adapter over product/default services such as
`Experiments`, `Analytics`, `LineageAdmin`, `Workflows`, and other typed
surfaces. If internal eval is exposed through MCP, it must remain qualified as
`internal-eval-runner` gated. MCP must not publish public `/v1/evals/*`
semantics, own experiment, eval, analytics, learning, workflow, or lineage
domain models, or bypass service-level authorization.

Grafana dashboards live in `dashboards/grafana/` and Prometheus alert rules live
in `ops/prometheus/alerts/`. Import the dashboards with a Postgres datasource
named `DS_POSTGRES` and a Prometheus datasource named `DS_PROMETHEUS`; the
workspace selector is populated from `analytics.turn_lineage`.

## Compliance Audit Tier

Compliance audit is an opt-in superset of the engineering lineage tier. A row
in `analytics.compliance_workspaces` enables workspace-local BLAKE3 chain links
on `analytics.turn_lineage`, periodic Merkle roots in `analytics.audit_roots`,
PII pseudonymization side data in `pii_vault`, and DSAR tooling through the
hosted `POST /v1/lineage/export`, `POST /v1/lineage/verify`, and
`POST /v1/lineage/erase` APIs. Workspaces that are not enabled keep the L01-L03
behavior and store `prev_hash = NULL`.

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
| `moa-brain` | Context pipeline, query rewrite, segment helpers, segment assessment |
| `moa-session` | Postgres session store, event log, task segments, learning log |
| `moa-memory/graph` (`moa-memory-graph`) | Graph-memory SQL sidecars, RLS, changelog, and AGE projection helpers |
| `moa-memory/ingest` (`moa-memory-ingest`) | Slow-path graph ingestion and fast memory write APIs |
| `moa-memory/pii` (`moa-memory-pii`) | PII classification and privacy helpers |
| `moa-memory/vector` (`moa-memory-vector`) | Graph-memory vector storage abstraction and pgvector backend |
| `moa-lineage/core` (`moa-lineage-core`) | Lineage records and score record types |
| `moa-lineage/citation` (`moa-lineage-citation`) | Provider citation normalization and answer-source verification |
| `moa-lineage/sink` (`moa-lineage-sink`) | Async lineage sink writers |
| `moa-lineage/otel` (`moa-lineage-otel`) | OTel/OpenInference bridge |
| `moa-lineage/audit` (`moa-lineage-audit`) | Compliance audit hashes, Merkle roots, signing, DSAR support |
| `moa-auth/authz-schema` (`moa-authz-schema`) | Typed FGA tuple keys and schema constants |
| `moa-auth/fga-bootstrap` (`moa-fga-bootstrap`) | Idempotent OpenFGA store and model bootstrap binary |
| `moa-auth/authz` (`moa-authz`) | FGA client, transactional outbox, and outbox poller |
| `moa-edge` | Public HTTP edge for token validation and identity header injection |
| `moa-auth/providers` (`moa-auth-providers`) | Local API keys, builtin approvals, null token vault, and provider bundle construction |
| `moa-auth/auth0` (`moa-auth-providers-auth0`) | Optional Auth0 and generic OIDC implementations gated by the `auth0` Cargo feature |
| `moa-ocsf` | OCSF v1.3 security-event types, emission helpers, and per-tenant signing |
| `moa-hands` | Tool routing and hand providers |
| `moa-providers` | LLM and embedding providers |
| `moa-orchestrator` | Restate handlers and cloud orchestration binary |
| `moa-gateway` | Messaging adapters and renderers |
| `moa-security` | Vault, policies, MCP credential proxy, injection controls |
| `moa-skills` | Skill parsing, distillation, improvement, regression generation |
| `moa-eval` | Evaluation harness |
| `moa-experiments` | Live behavior experiment domain model and scoped Postgres run ledger |
| `moa-loadtest` | Direct HTTP load-test tooling for hosted APIs |
| `workspace-hack` | Generated `cargo-hakari` dependency feature unification crate |
| `xtask` | Repo-local audit and maintenance commands |

## Where To Look Next

- Orchestration details: `docs/02-brain-orchestration.md` and `docs/12-restate-architecture.md`
- Memory details: `docs/04-memory-architecture.md`
- Shared type placement: `docs/15-architecture-policy.md`
- Event and segment schema: `docs/05-session-event-log.md`
- Live behavior experiments: `docs/eval/live-behavior-experiments.md`
- Context pipeline: `docs/07-context-pipeline.md`
- Multi-tenant learning: `docs/14-multi-tenancy-and-learning.md`
