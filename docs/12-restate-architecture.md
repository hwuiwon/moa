# 12 - Restate Architecture

_Durable execution on Restate services, virtual objects, and workflows._

## Purpose

Restate is MOA's durable execution engine. Postgres remains the product record
for sessions, events, memory, analytics, learning, lineage, and audit. Restate
owns orchestration state: queues, workflow progress, awakeables, retries, and
handler journals.

This document defines how MOA maps product concepts to Restate primitives and
what must stay out of Restate state.

## Primitive Selection

| Restate primitive | Use in MOA | Reason |
|---|---|---|
| Service | Stateless calls such as `ToolExecutor`, `LLMGateway`, `SessionStore`, `Authz`, `Analytics`, `Memory`, `Skills`, `Tenants` | Durable RPC with retries, no keyed state. |
| Virtual Object | `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO` | Single-writer-per-key semantics and small hot state. |
| Workflow | `TurnExecution`, `Consolidate`, `EvalRun` | One logical run per ID with explicit progress and completion. |

Use the weakest primitive that gives the needed correctness property. Do not
use a workflow for conversational actors; do not use virtual-object state as a
product database.

## MOA Mapping

| MOA concept | Restate shape | Key |
|---|---|---|
| Session | Virtual Object | `session_id` |
| Top-level turn | Workflow | `turn_id` |
| Sub-agent | Virtual Object | `sub_agent_id` |
| Tool execution | Service | none |
| LLM call | Service | none |
| Graph-memory ingestion | Virtual Object | ingestion key |
| Memory consolidation | Workflow | `workspace_id:logical_date` |
| Scheduled job | Virtual Object | job name |
| Human approval | Awakeable plus Postgres event | awakeable id |

Sessions and sub-agents are virtual objects because they receive multiple
messages over time. `TurnExecution` is a workflow because one turn should have
one observable durable run. Consolidation and eval replays are workflows for
the same reason.

## Runtime Flow

```text
client or edge
  -> Restate ingress
  -> Session VO
  -> TurnExecution workflow
  -> Context pipeline
  -> LLMGateway service
  -> ToolExecutor service
  -> SessionStore/Postgres events
  -> Session VO outcome callback
```

The `Session` VO serializes message admission, queue state, cancellation
requests, and outcome recording. It starts `TurnExecution` and returns quickly.
The workflow owns the long LLM/tool loop so read-only status, queueing, and
cancellation do not wait behind a long turn.

## Handler Surfaces

Current orchestrator surfaces are bound by `moa-orchestrator` at startup:

| Primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO` |
| Workflow | `TurnExecution`, `Consolidate`, `EvalRun` |
| Service | `AgentRegistry`, `AgentTemplates`, `Agents`, `AdminMaintenance`, `Analytics`, `Approvals`, `ApiKeys`, `Audit`, `Authz`, `GraphMemoryMaint`, `Health`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `WorkspaceStore`, `Whoami` |

When adding a handler, place it by ownership:

- User/session state that must serialize by key goes on a VO.
- Long one-shot work with a unique run ID goes in a workflow.
- Stateless reads, writes, and external calls go in services.

## State Ownership

Restate state should be small, replay-safe, and useful only for orchestration.

| State | Owner |
|---|---|
| Full session event history | Postgres `events` table |
| Session metadata and status record | Postgres, mirrored in VO hot state as needed |
| Pending message queue | `Session` VO |
| Current turn progress | `TurnExecution` workflow |
| Pending approval awakeable id | Workflow/VO state plus Postgres event |
| Tool result and assistant output | Postgres event log |
| Graph memory, vectors, changelog | Postgres |
| Learning log | Postgres |
| Security events | Postgres and audit shipper |
| Handler journal | Restate |

If a user, admin, customer, or audit export needs to query it later, store it
in Postgres. If only the in-flight handler needs it to recover, Restate state
is appropriate.

## Determinism Rules

Code inside Restate handlers must keep replay safety in mind:

- External side effects go through Restate service calls, workflows, or
  journaled `ctx.run` sections.
- Time, randomness, and generated IDs must use deterministic Restate helpers or
  be produced inside journaled blocks.
- Retried handlers must be idempotent or guarded by product-level idempotency
  keys.
- Do not perform direct network or filesystem side effects in replay-sensitive
  sections unless they are journaled.

## Approvals

Approvals use Restate awakeables for suspension and Postgres events for
visibility:

```text
tool call requires approval
  -> workflow creates awakeable
  -> approval request event is persisted with awakeable id
  -> UI/gateway renders request
  -> user decides
  -> approvals handler resolves awakeable
  -> blocked workflow resumes
```

Gateway processes never own pending-turn state. If a gateway restarts, it can
reconstruct pending approvals from Postgres and resolve the Restate awakeable
later.

Sub-agent approvals route through the parent user's approval surface. The
sub-agent owns its blocked awakeable; the parent session or approvals service
forwards the final decision.

## Cancellation

MOA supports both:

| Path | Use |
|---|---|
| Cooperative cancellation | User asks the session or turn to stop; workflow checks at deterministic boundaries and records a normal cancelled outcome. |
| Restate invocation cancellation | Operator hard-stops a stuck invocation through Restate admin APIs. |

Prefer cooperative cancellation for product flows because it preserves normal
events, cleanup, and hand teardown.

## Failure And Idempotency

Restate retries failed invocations according to handler configuration. Tool
calls still need product-level idempotency:

- read-only tools may retry freely;
- remote writes should use idempotency keys when available;
- non-idempotent calls should not be retried after a side effect is confirmed.

After retry exhaustion, operators should inspect the Restate invocation, trace,
and Postgres session events before resuming or cancelling. Do not hide repeated
handler failure behind unbounded application-level retry loops.

## Journal And Retention

Journals are for recovery and recent debugging, not long-term product history.
Keep completion retention short for high-volume services such as LLM and tool
calls. Keep longer retention only where it helps operations, such as approval
resolution, consolidation, or slow-path ingestion.

VO state persists until explicitly cleared. Session and sub-agent state should
be cleared after the product session is terminal and old enough that live
recovery is no longer needed.

## Deployment

`moa-orchestrator` is a normal HTTP handler service registered with Restate.
Production should run Restate as a durable cluster, keep the handler endpoint
internal, and expose public traffic through `moa-edge`.

Deployment requirements:

- Postgres/Neon for product data.
- Restate ingress/admin URLs for handler registration and invocation.
- Configured LLM and embedding provider credentials.
- Configured hand provider for code/tool execution.
- OTel, metrics, and logs wired before tenant traffic.

Graceful shutdown should flip readiness false, deregister or stop accepting new
handler traffic, drain in-flight invocations within the configured window, and
let Restate reassign anything that does not finish.

## Observability

Handler spans should include Restate identity plus MOA tenant/session/workspace
attributes. The useful diagnostic chain is:

1. Restate invocation id and handler.
2. Session id, turn id, tenant id, workspace id.
3. Postgres session events.
4. OTel trace and span links.
5. Provider/tool timing and retry counters.

Dashboards should separate Restate health, turn latency, LLM/provider behavior,
approval latency, tool execution, and sandbox fleet health.

## Local Development

Local development uses the same Restate-backed orchestrator as cloud mode.
Bring the compose stack up only when the task needs Postgres, Restate, OpenFGA,
edge, PII, audit shipper, or load-test services. Stop it with
`docker compose down` when finished unless the task explicitly requires keeping
services up.

`docs/02-brain-orchestration.md` describes the current boot sequence and the
turn flow implemented by `Session` plus `TurnExecution`.

## Current Decisions

1. Postgres is the system of record; Restate is the orchestration engine.
2. Sessions and sub-agents are virtual objects.
3. Top-level turns run in `TurnExecution` workflows keyed by turn ID.
4. Human approvals use awakeables plus persisted session events.
5. Product-visible events, learning, memory, lineage, and audit stay in
   Postgres.
6. Gateways and clients can always rebuild visible state from Postgres events.
