# 00 — Direction

_Product identity, principles, and differentiators._

## What MOA Is

MOA is a cloud-first, Rust-based, multi-tenant AI agent platform for teams. It runs durable agent sessions, executes tools through isolated hands, stores all product-visible state in Postgres/Neon, and improves from completed work.

The core product model is:

```text
Platform
  -> Tenant (team)
       -> Users
       -> Workspaces
       -> Task segments
       -> Intent taxonomy
       -> Learning log
       -> Workspace memory and skills
```

MOA is not just a chat wrapper. It is an execution platform with durable orchestration, an auditable event log, graph memory, automated task resolution scoring, and a tenant-scoped learning pipeline.

## What MOA Provides

- **Durable work:** sessions and sub-agents survive process restarts because Restate owns orchestration and Postgres owns product data.
- **Task segmentation:** conversations are split into discrete task segments so one long session can contain many independently tracked outcomes.
- **Resolution detection:** MOA scores whether each task segment resolved, partially resolved, failed, was abandoned, or remains unknown without requiring explicit user feedback.
- **Per-tenant intent learning:** tenants start with no intents; MOA proposes intents from conversation clusters and only uses active, tenant-approved intents for classification.
- **Learning log:** every learned pattern, resolution score, intent decision, memory update, and skill change can be audited and invalidated by version or batch.
- **Workspace memory:** graph memory stores facts, decisions, lessons, sources, and provenance while Postgres sidecars and pgvector provide retrieval.
- **Skills:** successful workflows can become reusable Agent Skills; ranking improves as segment outcomes accumulate.
- **Pluggable execution:** local hands, Docker, Daytona, E2B, and MCP servers are routed through one tool abstraction.
- **Multiple interfaces:** CLI, REST/gateway, and messaging adapters all talk to the same session model.

## Design Values

1. **Durability before cleverness.** A session should recover from crashes, pauses, and human approval waits without losing state.
2. **Inspectable state.** Product-visible facts live in queryable stores: Postgres tables, graph memory records, event records, and learning-log entries.
3. **Tenant control.** Learned behavior must be scoped and reviewable. Platform-wide defaults are libraries to adopt, not policies to impose.
4. **Blank-slate learning.** A new tenant should not inherit another team's assumptions. Useful behavior emerges from its own conversations and explicit admin choices.
5. **Small stable abstractions.** Traits in `moa-core` define the boundaries; implementations can differ between local and cloud runtimes.
6. **Progressive context.** The pipeline keeps stable prefix content cacheable and loads expensive dynamic context only when it matters.
7. **Least necessary tool access.** Hands and MCP tools are selected, approved, and isolated based on the task.

## Differentiators

MOA's differentiators are architectural, not cosmetic:

- **Restate-native agents:** sessions and sub-agents map to virtual objects with single-writer semantics and durable waits.
- **Segment-level analytics:** learning is based on task outcomes, not whole-session guesses.
- **Resolution-weighted improvement:** skills and future retrieval decisions can use measured success rates.
- **Tenant-owned taxonomies:** intent labels reflect a team's work patterns and admin review.
- **Auditable learning:** the learning log gives provenance, confidence, versions, and rollback hooks.
- **Graph memory plus database retrieval:** learned knowledge keeps provenance and bitemporal history while retrieval gets production-grade indexes and embeddings.

## Non-Goals

- MOA does not train a per-tenant model for intent classification. It uses embedding nearest-centroid classification over tenant intent centroids.
- MOA does not force a global intent catalog onto tenants. Catalog entries are opt-in.
- MOA does not keep durable product state only in Restate. Restate is orchestration state; Postgres is the product record.
- MOA does not bind agent work to a single front door. CLI, REST/gateway, and messaging adapters are peers over the same runtime model.
