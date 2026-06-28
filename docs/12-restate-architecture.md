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
| Service | Durable stateless calls such as `ActionReviews`, `AuthzChallenges`, `LearningReview`, `ToolExecutor`, `LLMGateway`, `SessionStore`, `Authz`, `Memory`, `Skills`, `Tenants` | Durable RPC with retries, no keyed state. |
| Virtual Object | `Session`, `SubAgent`, `Tenant`, `CronJob`, `IngestionVO` | Single-writer-per-key semantics and small hot state. |
| Workflow | `TurnExecution`, `SubAgentTurnExecution`, `ArtifactWorkflowExecution`, `KnowledgeSyncIngestion`, `Consolidate`, `ExperimentRun`, `ExperimentTrialRun` | One logical run per ID with explicit progress and completion. |

Use the weakest primitive that gives the needed correctness property. Do not
use a workflow for conversational actors; do not use virtual-object state as a
product database.

## MOA Mapping

| MOA concept | Restate shape | Key |
|---|---|---|
| Session | Virtual Object | `session_id` |
| Top-level turn | Workflow | `turn_id` |
| Sub-agent | Virtual Object | `sub_agent_id` |
| Sub-agent turn | Workflow | `turn_id` |
| Tool execution | Service | none |
| LLM call | Service | none |
| Graph-memory ingestion | Virtual Object plus Postgres ingestion claim rows | ingestion key |
| Memory consolidation | Workflow | `tenant_id:logical_date` |
| Tenant knowledge sync ingestion | Workflow plus Postgres active-run claims | sync run id |
| Scheduled job | Virtual Object | job name |
| Tenant action review | Service plus Postgres row/event | review id |
| Read-only analytics/whoami/audit/lineage reads | Direct edge handler | HTTP request |

Sessions and sub-agents are virtual objects because they receive multiple
messages over time. `TurnExecution` and `SubAgentTurnExecution` are workflows
because one admitted turn should have one observable durable run. Artifact
workflow execution, tenant knowledge sync ingestion, and consolidation are
workflows for the same reason. Hosted eval status is a Postgres row; it is not
a workflow unless the eval body gains real durable-step semantics.

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

Current orchestrator surfaces are bound by one `moa-orchestrator` production
binary at startup. Domain logic behind those handlers should stay in-process
behind application services, repositories, or domain crates.

Core production bindings:

| Primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `SubAgent`, `Tenant`, `CronJob`, `IngestionVO` |
| Workflow | `TurnExecution`, `SubAgentTurnExecution`, `ArtifactWorkflowExecution`, `KnowledgeSyncIngestion`, `Consolidate` |
| Service | `ActionReviews`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `Workflows`, `ActionPolicy` |

Feature-gated bindings:

| Feature | Additional bindings |
|---|---|
| `experiments` | `Experiments` service plus `ExperimentRun` and `ExperimentTrialRun` workflows |
| `internal-eval-runner` | `Eval` service |
| `skill-learning` | `SkillLearning` workflow |

Internal application boundaries for action reviews, builtin async-authz
challenges, learning review, experiments, privacy, provider routing, and memory
retrieval are extraction seams inside the monolith. Read-only analytics,
whoami, audit verification, and lineage explain/query/verify stay off Restate
and run as direct edge reads after authz. These are not a direction to create
internal network services.

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
| Current session turn progress | `TurnExecution` workflow |
| Current sub-agent turn progress | `SubAgentTurnExecution` workflow |
| Pending tenant action reviews | Postgres `tenant_action_reviews` rows |
| Detached sub-agent result waiters | `SubAgent` VO, resolved by child terminal delivery |
| Tool result and assistant output | Postgres event log |
| Graph memory, vectors, changelog | Postgres |
| Learning log | Postgres |
| Security events | Postgres and audit shipper |
| Hand leases and sandbox binding | Postgres `moa.hand_leases` |
| Runtime cache/pacing/message refs | Redis when configured; process-local memory only for fallback |
| Handler journal | Restate |

If a user, admin, customer, or audit export needs to query it later, store it
in Postgres. If only the in-flight handler needs it to recover, Restate state
is appropriate.

Kubernetes request routing is non-sticky. A follow-up request may land on any
edge or orchestrator replica, so Restate handler state cannot be replaced by
ordinary process memory. Process-local maps are allowed only as reconnect
caches, transport demultiplexing, or performance caches whose correctness owner
is Postgres, Restate, or explicitly configured Redis runtime cache.

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

## Action Reviews

Action reviews do not suspend root or sub-agent workflows. The turn workflow
stores the review request in Postgres, appends a session event, returns a
pending-review tool result to the model, and continues:

```text
tool call requires admin review
  -> workflow stores tenant action review
  -> action-review event is persisted
  -> pending-review tool result is appended
  -> tenant admin decides later through ActionReviews
```

Gateway processes never own pending review state. If a gateway restarts, it can
reconstruct pending tenant action reviews from Postgres.

Sub-agent tool calls use the same action-review path as root turns. A pending
tenant-admin review records product state in Postgres and returns a
pending-review tool result to the child turn; it does not create a blocked
sub-agent awakeable.

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
calls. Keep longer retention only where it helps operations, such as
action-review resolution, consolidation, or slow-path ingestion.

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
- Optional Redis for shared runtime cache coordination; without it the runtime
  cache backend is process-local and must not be used for global correctness.
- Configured LLM and embedding provider credentials.
- Configured hand provider for code/tool execution.
- OTel, metrics, and logs wired before tenant traffic.

Graceful shutdown should flip readiness false, deregister or stop accepting new
handler traffic, drain in-flight invocations within the configured window, and
let Restate reassign anything that does not finish.

## Observability

Handler spans should include Restate identity plus MOA tenant/session
attributes. The useful diagnostic chain is:

1. Restate invocation id and handler.
2. Session id, turn id, tenant id.
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
Sub-agent conversational state is held by `SubAgent`; each admitted child turn
runs in `SubAgentTurnExecution`, and detached waits use child-owned result
awakeables plus parent-cached terminal results instead of status polling.

## Current Decisions

1. Postgres is the system of record; Restate is the orchestration engine.
2. Sessions and sub-agents are virtual objects.
3. Top-level turns run in `TurnExecution` workflows keyed by turn ID.
4. Sub-agent turns run in `SubAgentTurnExecution` workflows keyed by turn ID.
5. Tenant action reviews use the `ActionReviews` service plus Postgres rows
   and events; they do not block turn workflows.
6. Product-visible events, learning, memory, lineage, and audit stay in
   Postgres.
7. Gateways and clients can always rebuild visible state from Postgres events.
