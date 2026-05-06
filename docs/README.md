# MOA Architecture Docs

These docs describe the current MOA architecture as implemented in the Rust workspace. Code remains the source of truth when a detail differs from an older design note.

## Reading Order

| # | Document | Covers |
|---|---|---|
| 00 | [Direction](00-direction.md) | Product identity, principles, and differentiators |
| 01 | [Architecture Overview](01-architecture-overview.md) | System diagram, trait map, data flow, workspace layout |
| 02 | [Brain Orchestration](02-brain-orchestration.md) | Restate session objects, sub-agents, turn loop, local runtime |
| 03 | [Communication Layer](03-communication-layer.md) | REST/gateway surfaces, CLI, approvals, observation |
| 04 | [Memory Architecture](04-memory-architecture.md) | Graph memory, privacy filtering, sidecar indexes, pgvector semantic retrieval, consolidation |
| 05 | [Session & Event Log](05-session-event-log.md) | Postgres event schema, task segments, replay, compaction |
| 06 | [Hands & MCP](06-hands-and-mcp.md) | Hand providers, tool routing, MCP, lazy provisioning |
| 07 | [Context Pipeline](07-context-pipeline.md) | Ordered context processors, query rewriting, prompt caching |
| 08 | [Security](08-security.md) | Credential isolation, sandbox tiers, prompt-injection mitigations |
| 09 | [Skills & Learning](09-skills-and-learning.md) | Agent Skills, skill ranking, distillation, unified learning log |
| 10 | [Technology Stack](10-technology-stack.md) | Crates, services, build targets, deployment dependencies |
| 11 | [Event Replay Runbook](11-event-replay-runbook.md) | Operational replay and recovery procedures |
| 12 | [Restate Architecture](12-restate-architecture.md) | Restate primitives, handler mapping, deployment strategy |
| 13 | [Task Segmentation](13-task-segmentation.md) | Segment lifecycle, resolution scoring, analytics views |
| 14 | [Multi-Tenancy & Learning](14-multi-tenancy-and-learning.md) | Tenant model, adaptive intents, catalog, audit and rollback |

Supporting notes:

| Document | Covers |
|---|---|
| [Analytics](analytics.md) | Session, tool, and task-segment analytics views |
| [Implementation Caveats](implementation-caveats.md) | Known implementation tradeoffs and follow-up seams |
| [Prompt Caching Architecture](prompt-caching-architecture.md) | Cache-region rules and verification |
| [Approval Check](approval-check.md) | Approval behavior notes |
| [Event Fanout](event-fanout.md) | Event broadcast and observation behavior |
| [Observability](observability/) | Dashboard and metric notes |

## Current Architectural Decisions

| # | Decision | Status |
|---|---|---|
| 1 | Rust workspace with explicit crate boundaries around core traits, brain, session storage, memory, hands, providers, orchestration, gateway, security, skills, eval, and CLI. | Implemented |
| 2 | Restate is the durable cloud orchestration engine. Sessions and sub-agents are virtual objects; consolidation and intent discovery are workflows. | Implemented |
| 3 | Local mode uses `moa-orchestrator-local`, a Tokio-task orchestrator sharing the same core brain/session/graph-memory abstractions. | Implemented |
| 4 | Postgres is the single application database. Neon is the managed/cloud Postgres target and optional checkpoint branch provider. | Implemented |
| 5 | Session events are append-only and replayable. Derived counters live in triggers, generated columns, views, and materialized views. | Implemented |
| 6 | Graph memory is canonical; Postgres stores graph state, sidecar indexes, changelog rows, and pgvector embeddings. | Implemented |
| 7 | Query rewriting is a fail-open context pipeline processor that normalizes the current task, extracts high-level intent, and detects new task segments. | Implemented |
| 8 | Sessions are split into task segments with independent intent metadata, tool/skill usage, token cost, and resolution outcomes. | Implemented |
| 9 | Resolution detection is automated and signal-based: tool outcomes, verification commands, continuation signals, agent self-assessment, and structural baselines. | Implemented |
| 10 | Tenants start with blank intent taxonomies. Intent discovery proposes labels from tenant conversations; admin confirmation activates them. | Implemented |
| 11 | Global catalog intents are opt-in. No tenant receives platform-curated intents unless adopted or manually created. | Implemented |
| 12 | Learning is recorded in a bitemporal append-only `learning_log` with provenance, confidence, batch IDs, and invalidation via `valid_to`. | Implemented |
| 13 | Skills are ranked with a mix of keyword relevance, resolution rate, use count, and recency, with prompt-budget controls. | Implemented |
| 14 | CLI and REST/gateway surfaces are separate product interfaces over the same runtime model. | Implemented |

## Consistency Rules

- Do not introduce new durable orchestration primitives outside Restate without updating `02` and `12`.
- Do not add a second application database. New product state belongs in Postgres unless a doc explicitly records an exception.
- Tenant-level learning state belongs at tenant scope; workspace memory and skills remain workspace-scoped unless intentionally promoted.
- Any new learned behavior should write a `learning_log` entry with source references and actor identity.
