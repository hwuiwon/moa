# MOA Architecture

This is the high-level map of the current MOA system. The detailed specs live in
[`docs/`](docs/); code remains the source of truth when an older design note
drifts.

MOA is now aimed at enterprise agent operations, not a personal desktop agent.
The platform model is multi-tenant, auditable, Restate-backed, and Postgres-first.
Local mode exists so engineers can develop and operate the same runtime model
from the CLI, not as a separate consumer product.

---

## 1. Mental Model

MOA has four durable boundaries:

1. **Runtime boundary:** clients talk to either the local Tokio orchestrator or
   the cloud Restate handler service.
2. **Brain boundary:** `moa-brain` compiles context, calls model providers, runs
   approval/tool loops, scores task resolution, and emits lineage.
3. **Execution boundary:** `moa-hands` routes built-in tools, local/Docker
   execution, Daytona, E2B, and MCP through one tool router.
4. **Data boundary:** Postgres/Neon owns the product record: sessions, events,
   graph memory, vectors, task segments, tenant intents, learning log, lineage,
   analytics, and audit rows.

Restate owns durable cloud orchestration. Postgres owns enterprise state and
auditability.

---

## 2. System Diagram

```text
Clients
  CLI / daemon
  REST and gateway surfaces
  Telegram / Slack / Discord adapters
        |
        v
Runtime boundary
  Local: moa-orchestrator-local
  Cloud: moa-orchestrator-bin Restate handler service
        |
        v
Durable orchestration
  Session VO       -> user session actor
  SubAgent VO      -> delegated worker actor
  Workspace VO     -> workspace coordination
  IngestionVO      -> durable graph-memory ingestion
  Services         -> SessionStore, LLMGateway, ToolExecutor,
                      IntentManager, WorkspaceStore, Health
  Workflows        -> Consolidate, IntentDiscovery
        |
        v
Brain and execution
  context pipeline -> provider router -> Anthropic / OpenAI / Gemini
  ToolRouter       -> built-ins / local / Docker / Daytona / E2B / MCP
        |
        v
Postgres / Neon
  sessions, events, pending_signals, context_snapshots
  task_segments, analytics views, skill_resolution_rates
  graph nodes, graph edges, sidecar indexes, changelog, pgvector
  tenant_intents, global_intent_catalog, learning_log
  analytics.turn_lineage, analytics.scores, compliance audit tables
```

---

## 3. Enterprise Product Model

```text
Platform
  -> Tenant
       -> Users
       -> Workspaces
       -> Sessions
       -> Task segments
       -> Intent taxonomy
       -> Learning log
       -> Workspace memory
       -> Workspace skills
       -> Lineage, analytics, and audit evidence
```

Enterprise behavior is tenant-controlled:

- New tenants start with an empty intent taxonomy.
- Platform catalog intents are opt-in.
- Learning is append-only and invalidated by `valid_to`, not silently rewritten.
- Workspace memory and skills remain scoped; ranking signals aggregate at tenant
  level where the work pattern is team-level.
- Compliance audit is an opt-in tier with explicit attestation caveats until
  external cryptographic review is complete.

---

## 4. Core Traits

Stable interfaces live in [`crates/moa-core`](crates/moa-core/).

| Trait | Responsibility | Current implementations |
|---|---|---|
| `BrainOrchestrator` | Start, resume, signal, list, and observe sessions | `LocalOrchestrator`; Restate objects/services |
| `SessionStore` | Append-only event log, sessions, signals, snapshots, task segments, analytics, learning | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `FileBlobStore` |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `LLMProvider` | Completion and streaming provider interface | Anthropic, OpenAI, Gemini, scripted tests |
| `EmbeddingProvider` | Shared embedding interface | OpenAI embedding, Cohere v4, Gemini embedding, mock/test embedding |
| `HandProvider` | Provision, execute, pause, resume, destroy execution environments | local, Docker, Daytona, E2B |
| `BuiltInTool` | In-process tools with policy and schema metadata | memory, file/search/read/write, shell helpers |
| `PlatformAdapter` | Gateway normalization/rendering | Telegram, Slack, Discord |
| `ContextProcessor` | Ordered context-pipeline stage | identity, instructions, tools, skills, query rewrite, memory, history, runtime context, compactor, cache |
| `CredentialVault` | Secret storage abstraction | encrypted local file vault, environment-backed MCP vault |
| `LineageHandle` | Transport-neutral lineage capture | null handle, async sink/OTel bridge |

---

## 5. Workspace Layout

| Crate | Role |
|---|---|
| `moa-core` | Shared types, traits, config, events, telemetry, analytics DTOs |
| `moa-brain` | Context pipeline, retrieval, turn harness, approvals, resolution scoring, lineage emission |
| `moa-session` | Postgres session store, event log, snapshots, task segments, intents, learning log, analytics |
| `moa-memory-graph` | Graph memory, AGE projection helpers, sidecars, RLS, changelog |
| `moa-memory-ingest` | Slow-path ingestion and fast memory write APIs |
| `moa-memory-pii` | PII classification and memory privacy helpers |
| `moa-memory-vector` | pgvector and Turbopuffer vector stores |
| `moa-lineage-core` | Lineage record types and score records |
| `moa-lineage-sink` | Async lineage sink writers |
| `moa-lineage-otel` | OTel/OpenInference bridge |
| `moa-lineage-citation` | Citation/provenance adapters |
| `moa-lineage-cold` | Cold storage partition/export support |
| `moa-lineage-audit` | Compliance audit hashes, roots, signing, and DSAR support |
| `moa-hands` | Tool router and execution adapters |
| `moa-providers` | Provider core and vendor adapters |
| `moa-orchestrator` | Restate objects, services, workflows, and `moa-orchestrator-bin` |
| `moa-orchestrator-local` | Local Tokio-task orchestrator |
| `moa-gateway` | Messaging adapters and platform renderers |
| `moa-runtime` | Shared runtime assembly |
| `moa-cli` | `moa` CLI and local daemon |
| `moa-security` | Vaults, MCP credential proxy, policies, prompt-injection controls |
| `moa-skills` | Agent Skills parsing, distillation, improvement, and regression generation |
| `moa-eval` | Evaluation harness |
| `moa-loadtest` | Load-test harness |
| `workspace-hack` | Generated `cargo-hakari` feature unification crate |
| `xtask` | Repo-local audit and maintenance commands |

---

## 6. Runtime Modes

### Local Development And Operator Mode

```text
CLI / daemon
  -> moa-orchestrator-local
  -> moa-brain
  -> Postgres dev stack on localhost:10040
  -> local/Docker hands and configured providers
```

Local development uses `docker-compose.yml` for Postgres 17 with AGE, pgvector,
pgaudit, the PII service, and the audit shipper. `~/.moa/config.toml` plus
`MOA__...` overrides configure the CLI/runtime. This is the fastest way to test
the enterprise runtime without Restate.

### Cloud Runtime

```text
Restate
  -> moa-orchestrator-bin handler endpoint
  -> Postgres/Neon product database
  -> configured LLM providers
  -> configured hand provider and MCP servers
  -> observability, lineage, metrics, audit sinks
```

The orchestrator binary reads cloud process settings from environment variables:

```bash
POSTGRES_URL=postgres://...
RESTATE_ADMIN_URL=http://localhost:10011
OPENAI_API_KEY=... # or ANTHROPIC_API_KEY / GOOGLE_API_KEY
cargo run -p moa-orchestrator --bin moa-orchestrator-bin -- --port 10020 --health-port 10021
```

The Docker image builds `moa-orchestrator-bin` and installs it as
`/usr/local/bin/moa-orchestrator`.

---

## 7. Turn Flow

```text
User message
  -> gateway or CLI normalizes input
  -> Session VO / LocalOrchestrator appends UserMessage
  -> context pipeline runs
       1 identity
       2 instructions
       3 tools
       4 skills
       5 query_rewrite
       6 memory
       7 history
       8 runtime_context
       9 compactor
      10 cache
  -> provider router selects model for the task
  -> LLM response streams through the harness
  -> tool calls route through ToolExecutor / ToolRouter
  -> approvals pause durably until a user/admin decision arrives
  -> events, tool outputs, lineage, metrics, and segment counters persist
  -> resolution scoring and learning entries update tenant signals
```

Replay is the recovery model. If the runtime restarts, durable state is rebuilt
from Postgres events plus Restate journals.

---

## 8. Data Planes

| Plane | Primary owner | Notes |
|---|---|---|
| Session/event log | `moa-session` | Append-only event history and queryable session state |
| Task analytics | `moa-session` | Segments, skill rates, intent transitions, materialized views |
| Memory graph | `moa-memory-graph` | Bitemporal graph records, sidecars, changelog, RLS scope |
| Vector retrieval | `moa-memory-vector` | pgvector default; Turbopuffer promotion path |
| Memory ingestion | `moa-memory-ingest` | Slow-path ingestion and deterministic fast writes |
| Privacy | `moa-memory-pii` | PII classification before memory writes |
| Lineage | `moa-lineage-*` | Hot lineage, scores, OTel bridge, cold export, audit tier |
| Orchestration | Restate | VO/workflow state and invocation journals only |

---

## 9. Security And Governance

Default enterprise posture:

- Product-visible state is tenant/workspace scoped in Postgres.
- Tools are routed through explicit schemas and policies.
- Risky write/execute operations request approval.
- MCP credentials are proxied; secrets do not enter LLM-generated code.
- Local hands are convenient for development; cloud deployments should use
  container or microVM isolation for code execution.
- Compliance audit is opt-in and clearly separated from engineering lineage.

See [`docs/08-security.md`](docs/08-security.md) and
[`docs/01-architecture-overview.md`](docs/01-architecture-overview.md).

---

## 10. Where To Look When Things Break

| Symptom | First stop |
|---|---|
| Session stuck or not resuming | [`docs/02-brain-orchestration.md`](docs/02-brain-orchestration.md), [`docs/12-restate-architecture.md`](docs/12-restate-architecture.md) |
| Event replay mismatch | [`docs/11-event-replay-runbook.md`](docs/11-event-replay-runbook.md) |
| Tool approval or sandbox issue | [`docs/06-hands-and-mcp.md`](docs/06-hands-and-mcp.md), [`docs/08-security.md`](docs/08-security.md) |
| Context cost/cache regression | [`docs/07-context-pipeline.md`](docs/07-context-pipeline.md), [`docs/prompt-caching-architecture.md`](docs/prompt-caching-architecture.md) |
| Memory retrieval or ingestion issue | [`docs/04-memory-architecture.md`](docs/04-memory-architecture.md) |
| Tenant learning or intent issue | [`docs/14-multi-tenancy-and-learning.md`](docs/14-multi-tenancy-and-learning.md) |
| Lineage/audit issue | [`docs/ops/audit-runbook.md`](docs/ops/audit-runbook.md), [`docs/operations/subject-access-runbook.md`](docs/operations/subject-access-runbook.md) |

---

## 11. Design Values

- **Enterprise inspectability over magic.** Every learned behavior needs
  provenance and rollback.
- **Durability before cleverness.** Long waits, approvals, and restarts are normal.
- **Tenant control.** Platform defaults are libraries to adopt, not policy forced
  across tenants.
- **Small seams.** Traits in `moa-core` keep local/cloud and vendor adapters
  replaceable.
- **Least necessary tool access.** Tool execution is selected, approved, and
  isolated based on risk.
