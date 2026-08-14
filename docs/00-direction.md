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

- **Durable work at every horizon:** sessions and conversational workers use
  Restate directly; execution runs persist their complete product state in
  Postgres and advance through bounded Restate controller, task-attempt,
  compensation-attempt, and trigger activations. A run waiting for input,
  review, signal, time, external completion, or operator resume retains no live
  handler or sandbox compute.
- **Task segmentation:** conversations are split into discrete task segments so one long session can contain many independently tracked outcomes.
- **Outcome assessment:** MOA records whether each task segment resolved, partially resolved, failed, was abandoned, or remains unknown without requiring explicit user feedback.
- **Per-tenant learning:** task outcomes become experience records, attributions, candidates, skill changes, and memory updates at tenant scope without requiring a fixed session intent taxonomy.
- **Learning log:** every learned pattern, segment assessment, memory update, and skill change can be audited and invalidated by version or batch.
- **Lineage and audit:** retrieval, context, generation, scores, DSAR exports, and optional compliance audit records are first-class operational artifacts.
- **Tenant knowledge:** relational graph memory stores facts, decisions, lessons, sources, and provenance as Postgres nodes and edges, while sidecar indexes and the configured vector backend provide retrieval. Contact memory is contact-local and does not inherit tenant memory or another contact's memory.
- **Dynamic execution:** each request is routed once to `Respond`, `Execute`, or `NeedsInput`; Execute derives an `Inline` or `Durable` strategy, and only an initial root Inline turn may make one evidence-preserving upgrade to Durable.
- **Skills:** skills are optional execution inputs, never routes or admission gates. Custom instruction-only Agent Skills work in Inline Execute and in Durable `Agent` nodes. An activated skill may also carry a pinned reusable `execution_plan` template; one-off compiled plans remain immutable run snapshots and are never auto-published.
- **Tenant ownership:** skills, policies, and connector connections are tenant-owned runtime data.
- **Pluggable execution:** local hands, Docker, Daytona, E2B, operator MCP servers, and reviewed tenant HTTP connector actions are routed through one governed tool boundary. Tenant knowledge sync remains a separate Nango/Merge provider flow and never becomes a model tool.
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
9. **Durability is explicit.** A durable session or Restate journal does not make arbitrary sandbox files durable. Compute, logical workspace metadata, and committed filesystem bytes have distinct owners and lifecycles.

## Differentiators

MOA's differentiators are architectural, not cosmetic:

- **Restate-native agents:** sessions and workers map to virtual objects with single-writer semantics and durable waits.
- **Reliable long-horizon execution:** `ExecutionRunController`,
  `ExecutionTaskAttempt`, and `ExecutionTrigger` execute validated plans through
  bounded, generation-fenced activations. Postgres owns plan, identity, wait,
  task, budget, schedule, external-job, and outbox truth; fleet/tenant admission
  bounds active compute separately from storage-only parked work.
- **Experience-level analytics:** learning is derived from assessed task segments, not whole-session guesses.
- **Resolution-weighted improvement:** skills and future retrieval decisions can use measured success rates.
- **Candidate-gated adaptation:** reusable skills, memory proposals, policy proposals, and eval proposals start as learning candidates before promotion.
- **Auditable learning:** the learning log gives provenance, confidence, versions, and rollback hooks.
- **Operational evidence:** lineage, scores, analytics, and compliance audit tiers let operators explain what happened without scraping logs.
- **Graph memory plus database retrieval:** learned knowledge keeps provenance and bitemporal history while retrieval gets production-grade indexes and embeddings.

## Non-Goals

- MOA does not require a durable session intent taxonomy for routing or learning. The agent loop and skills decide dynamically from context.
- MOA does not keep durable product state only in Restate. Restate is orchestration state; Postgres is the product record.
- MOA does not retain sandbox process memory by default or treat a live sandbox, mutable volume, paused instance, or provider snapshot as the committed filesystem revision. The filesystem-only workspace contract requires a verified portable checkpoint.
- MOA does not represent a day- or week-scale execution as one lifetime-spanning
  workflow invocation. Every activation returns after bounded progress; every
  yield checkpoints required filesystem state and destroys active compute.
- MOA does not bind agent work to a single front door. REST/gateway, API automation, and messaging adapters are peers over the same runtime model.
- MOA does not optimize for a single-user personal desktop workflow. Local mode is a development and operator path over the same enterprise runtime model.
