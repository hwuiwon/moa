# 00 — Direction

_Product identity, principles, and differentiators._

## What MOA Is

MOA is a cloud-first, Rust-based, multi-tenant AI agent operations platform for enterprises. It runs durable agent sessions, executes tools through isolated hands, stores product-visible and audit state in Postgres/Neon, and improves from completed work under tenant control.

The core product model is:

```text
workspace
  -> tenant
       -> contact
            -> session
```

The workspace is a single deployment container. The tenant is the hard runtime
isolation boundary. Contacts are end users inside a tenant, while users are
admin/operator principals who manage or operate tenants.

MOA is not a personal assistant or chat wrapper. It is an execution platform with durable orchestration, an auditable event log, graph memory, evidence-backed task segment assessment, and a tenant-scoped learning pipeline.

## What MOA Provides

- **Durable work:** sessions, conversational workers, and execution runs survive process restarts because Restate owns orchestration and Postgres owns product data.
- **Task segmentation:** conversations are split into discrete task segments so one long session can contain many independently tracked outcomes.
- **Outcome assessment:** MOA records whether each task segment resolved, partially resolved, failed, was abandoned, or remains unknown without requiring explicit user feedback.
- **Per-tenant learning:** task outcomes become experience records, attributions, candidates, skill changes, and memory updates at tenant scope without requiring a fixed session intent taxonomy.
- **Learning log:** every learned pattern, segment assessment, memory update, and skill change can be audited and invalidated by version or batch.
- **Lineage and audit:** retrieval, context, generation, scores, DSAR exports, and optional compliance audit records are first-class operational artifacts.
- **Tenant knowledge:** relational graph memory stores facts, decisions, lessons, sources, and provenance as Postgres nodes and edges, while sidecar indexes and the configured vector backend provide retrieval. Contact memory is contact-local and does not inherit tenant memory or another contact's memory.
- **Dynamic execution:** each request takes the cheapest reliable shape: a direct `respond`, a bounded tool-using `act`, or a durable `run` over a validated typed DAG.
- **Skills:** instruction-only Agent Skills remain first-class. A published skill may also carry a pinned reusable `execution_plan` template; one-off compiled plans remain immutable run snapshots and are never auto-published.
- **Tenant ownership:** skills and policies are tenant-owned runtime data.
- **Pluggable execution:** local hands, Docker, Daytona, E2B, and MCP servers are routed through one tool abstraction.
- **Multiple interfaces:** REST/gateway, API automation, and messaging adapters all talk to the same session model.

## Design Values

1. **Durability before cleverness.** A session should recover from crashes, pauses, and human approval waits without losing state.
2. **Inspectable state.** Product-visible facts live in queryable stores: Postgres tables, graph memory records, event records, and learning-log entries.
3. **Tenant control.** Learned behavior must be scoped and reviewable. Tenant learning is tenant-local and never globally promoted.
4. **Blank-slate learning.** A new tenant should not inherit another team's assumptions. Useful behavior emerges from its own conversations and explicit admin choices.
5. **Small stable abstractions.** Traits in `moa-core` define the boundaries; implementations can differ between local and cloud runtimes.
6. **Progressive context.** The pipeline keeps stable prefix content cacheable and loads expensive dynamic context only when it matters.
7. **Least necessary tool access.** Hands and MCP tools are selected, approved, and isolated based on the task.
8. **Enterprise governance.** Admin operations, audit trails, and rollback paths are part of the product surface, not post-hoc logs.

## Differentiators

MOA's differentiators are architectural, not cosmetic:

- **Restate-native agents:** sessions and workers map to virtual objects with single-writer semantics and durable waits.
- **Reliable bulk execution:** `ExecutionRun` and `ExecutionTask` durably execute validated plans with atomic budgets, exact coverage, and automatic terminal delivery to the owning session.
- **Experience-level analytics:** learning is derived from assessed task segments, not whole-session guesses.
- **Resolution-weighted improvement:** skills and future retrieval decisions can use measured success rates.
- **Candidate-gated adaptation:** reusable skills, memory proposals, policy proposals, and eval proposals start as learning candidates before promotion.
- **Auditable learning:** the learning log gives provenance, confidence, versions, and rollback hooks.
- **Operational evidence:** lineage, scores, analytics, and compliance audit tiers let operators explain what happened without scraping logs.
- **Graph memory plus database retrieval:** learned knowledge keeps provenance and bitemporal history while retrieval gets production-grade indexes and embeddings.

## Non-Goals

- MOA does not require a durable session intent taxonomy for routing or learning. The agent loop and skills decide dynamically from context.
- MOA does not keep durable product state only in Restate. Restate is orchestration state; Postgres is the product record.
- MOA does not bind agent work to a single front door. REST/gateway, API automation, and messaging adapters are peers over the same runtime model.
- MOA does not optimize for a single-user personal desktop workflow. Local mode is a development and operator path over the same enterprise runtime model.
