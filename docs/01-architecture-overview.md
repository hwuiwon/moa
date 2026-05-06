# 01 — Architecture Overview

_System model, trait map, data flow, and workspace layout._

## System Model

```text
Clients
  GPUI desktop | CLI | REST/gateway | Telegram/Slack/Discord
        |
        v
Runtime boundary
  Local: `moa-orchestrator-local` Tokio tasks
  Cloud: `moa-orchestrator` Restate handler service
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
        |
        v
Learning loop
  segments -> resolution scores -> learning log
  learning log -> intent proposals, skill ranking, memory consolidation
```

Restate owns durable cloud execution. Postgres owns product-visible data. Graph memory is the canonical memory source, with sidecar and vector indexes maintained by graph writes.

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
```

Intent taxonomies are tenant-scoped because teams tend to repeat work patterns across projects. Memory and skills remain workspace-scoped, but ranking signals aggregate at tenant level.

## Core Traits

Current trait definitions live in `crates/moa-core/src/traits.rs`; shared DTOs live under `crates/moa-core/src/types/`.

| Trait | Purpose | Main implementations |
|---|---|---|
| `BrainOrchestrator` | Start, resume, signal, list, observe sessions; schedule background work | `LocalOrchestrator`; Restate services/objects through `moa-orchestrator` |
| `SessionStore` | Append-only event log, sessions, pending signals, snapshots, task segments, analytics, skill rates | `PostgresSessionStore` |
| `BlobStore` | Claim-check storage for large session artifacts | `FileBlobStore` |
| `BranchManager` | Optional database checkpoint branches | `NeonBranchManager` |
| `HandProvider` | Provision, execute, pause/resume, destroy hands | local, Docker, Daytona, E2B |
| `LLMProvider` | Provider completion interface | Anthropic, OpenAI, Gemini through `moa-providers` |
| `PlatformAdapter` | Gateway inbound/outbound normalization | Telegram, Slack, Discord |
| `BuiltInTool` | Built-in tool execution | memory/search/web and other built-ins |
| `ContextProcessor` | One stage in context compilation | identity, instructions, tools, skills, query rewrite, memory, history, runtime context, compactor, cache |
| `CredentialVault` | Secret storage and retrieval | local encrypted vault; cloud vault integration |

The local and cloud runtimes share these seams. They differ in how turns are scheduled and recovered.

## Runtime Modes

### Cloud

`moa-orchestrator` exposes Restate handlers:

- Virtual objects: `Session`, `SubAgent`, `Workspace`
- Services: `Health`, `SessionStore`, `IntentManager`, `LLMGateway`, `ToolExecutor`, `WorkspaceStore`
- Workflows: `Consolidate`, `IntentDiscovery`

`Session` is the durable actor for one session key. It queues messages, calls `run_turn`, tracks the active task segment, records tool/skill usage, scores resolution, and writes learning entries. `SubAgent` is the same actor pattern for delegated work with depth and budget limits.

### Local

`moa-orchestrator-local` runs the same brain loop in Tokio tasks with broadcast channels for observation. It is used by `moa-cli`, `moa-runtime`, and the desktop app. It still uses Postgres for session storage and the same graph memory and retrieval infrastructure.

## Turn Data Flow

```text
User message
  -> SessionStore emits `UserMessage`
  -> Session VO or local task prepares a turn
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

## Workspace Layout

| Crate | Role |
|---|---|
| `moa-core` | Shared types, traits, config, events, analytics helpers |
| `moa-brain` | Context pipeline, query rewrite, segment helpers, intent classifier, resolution scoring |
| `moa-session` | Postgres session store, event log, task segments, intents, learning log |
| `moa-memory-graph` | Graph-memory SQL sidecars, RLS, changelog, and AGE projection helpers |
| `moa-memory-ingest` | Slow-path graph ingestion and fast memory write APIs |
| `moa-memory-pii` | PII classification and privacy helpers |
| `moa-memory-vector` | Graph-memory vector storage abstraction and pgvector backend |
| `moa-hands` | Tool routing and hand providers |
| `moa-providers` | LLM and embedding providers |
| `moa-orchestrator` | Restate handlers and cloud orchestration binary |
| `moa-orchestrator-local` | Tokio-task local orchestrator |
| `moa-gateway` | Messaging adapters and renderers |
| `moa-runtime` | Shared runtime assembly |
| `moa-cli` | CLI and daemon entrypoints |
| `moa-security` | Vault, policies, MCP credential proxy, injection controls |
| `moa-skills` | Skill parsing, distillation, improvement, regression generation |
| `moa-eval` | Evaluation harness |
| `moa-loadtest` | Load-test tooling |
| `moa-desktop` | GPUI desktop app |

## Where To Look Next

- Orchestration details: `docs/02-brain-orchestration.md` and `docs/12-restate-architecture.md`
- Memory details: `docs/04-memory-architecture.md`
- Event and segment schema: `docs/05-session-event-log.md`
- Context pipeline: `docs/07-context-pipeline.md`
- Multi-tenant learning: `docs/14-multi-tenancy-and-learning.md`
