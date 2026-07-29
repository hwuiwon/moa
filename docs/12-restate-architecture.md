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
| Service | Durable stateless calls such as `ActionReviews`, `AuthzChallenges`, `Execution`, `LearningReview`, `ToolExecutor`, `LLMGateway`, `SecurityEvents`, `SessionStore`, `Authz`, `Memory`, `Skills`, `Tenants` | Durable RPC with retries, no keyed state. |
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO` | Single-writer-per-key semantics and small hot state. |
| Workflow | `TurnExecution`, `WorkerTurnExecution`, `ExecutionRun`, `ExecutionTask`, `KnowledgeSyncIngestion`, `KnowledgeIndexRebuild`, `Consolidate`, `ExperimentRun`, `ExperimentTrialRun` | One logical run or task per ID with explicit progress and completion. |

Use the weakest primitive that gives the needed correctness property. Do not
use a workflow for conversational actors; do not use virtual-object state as a
product database.

## MOA Mapping

| MOA concept | Restate shape | Key |
|---|---|---|
| Session | Virtual Object | `session_id` |
| Top-level turn | Workflow | `turn_id` |
| Worker | Virtual Object | `worker_id` |
| Worker turn | Workflow | `turn_id` |
| Execution run | Workflow plus `moa.execution_run` | `run_uid` |
| Execution task | Workflow plus `moa.execution_task` | stable hash of `(run_uid, node_id, item_key)` |
| Tool execution | Service | none |
| LLM call | Service | none |
| Graph-memory ingestion | Virtual Object plus Postgres ingestion claim rows | ingestion key |
| Memory consolidation | Workflow | `tenant_id:logical_date` |
| Tenant knowledge sync ingestion | Workflow plus Postgres active-run claims | sync run id |
| Scheduled job | Virtual Object | job name |
| Tenant action review | Service plus Postgres row/event | review id |
| Read-only analytics/whoami/audit/lineage reads | Direct edge handler | HTTP request |

Sessions and workers are virtual objects because they receive multiple
messages over time. `TurnExecution` and `WorkerTurnExecution` are workflows
because one admitted turn should have one observable durable run. `ExecutionRun`
and `ExecutionTask` are workflows because typed graph work has stable run/task
identities, durable waits, recovery, and explicit terminal outcomes. Tenant
knowledge sync ingestion and consolidation are workflows for the same reason.
Hosted eval status is a Postgres row; it is not a workflow unless the eval body
gains real durable-step semantics.

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
`start_turn` and `queue_message` are the only message-submitting handlers; both pass
through one admission fence held in the VO's `message_admissions` state key, which records
each admitted `client_message_id` with the canonical hash of its request and the exact
response the caller received. The fence is consulted before every side effect — reply
delivery, queue mutation, shared admission lease, turn dispatch — so a retried submission
replays its original response, and one id reused for a different request is refused. A
queued admission keeps replaying its `queued` response even after the queue starts its
turn, because that is what its caller was told; the admission becomes terminal only when
its turn or reply reaches a terminal disposition, and is then retained for the earlier of
24 hours or 256 newer terminal admissions.
The workflow owns the long LLM/tool loop so read-only status, queueing, and
cancellation do not wait behind a long turn.

After context compilation, `TurnExecution` selects exactly one public route:
Respond, Execute, or NeedsInput. Respond makes one no-tool model call.
NeedsInput emits one bounded deterministic clarification. Execute carries one
explicit internal strategy: Inline retains the bounded root tool loop and
optional conversational Worker delegation; Durable persists an
immutable goal contract and canonical plan, starts `ExecutionRun` detached, and
returns without making the root model poll status. A bounded free-form
classifier rationale may accompany the active turn, but it is never a workflow
control input and is not persisted in route audits, runs, or analytics.

Only an initial root user-message Execute/Inline turn can make one typed,
evidence-preserving upgrade to Durable. It does not classify again, cannot
downgrade, and child-signal or worker-result continuations remain Inline. The
workflow injects `request_durable_execution` only into that eligible turn,
requires the control tool to be called alone, and rejects arbitrary tool-result
payloads as upgrade authority. A terminal run requests one guarded compact
synthesis turn on the owning session.
Successful Durable admission returns the terminal root-turn outcome `Accepted`,
publishes one minimal `ExecutionRunStarted` event, and keeps the owning Session
`Running` while detached execution continues.

Skills are optional execution inputs, not routing decisions or admission gates.
Custom instruction-only skills work in Inline Execute and in declared Durable
`Agent` nodes; neither selection nor absence changes the public route.

## Admission Control

`moa-edge` forwards every public request to the Restate ingress using the v1.7
path scheme. Request-response calls use `POST /restate/call/{service}/{handler}`
(or `.../{service}/{key}/{handler}` for a keyed virtual object such as
`Session`); the deprecated unversioned `/{Service}/{key}/{handler}` form is gone.
The edge only issues request-response calls, so it never uses the fire-and-forget
`/restate/send/...` form.

Restate 1.7 flow control (experimental vqueues,
`RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true`) caps concurrent invocations per
**scope**. A scope is a single opaque path segment on the scoped ingress form,
`POST /restate/scope/{scopeKey}/call/{service}/{key}/{handler}`. MOA's scope-key
convention is `tenant-{tenant_id}` (the tenant UUID is one segment). The edge
tags only the invocations that start expensive agent work — posting a message
(`Contacts/send_message`, which queues on the `Session` VO and starts a turn) —
with the tenant scope. Cheap reads and status polls (`Session/progress`,
`Contacts/progress`, `Contacts/authorize_session`), session lifecycle calls
(`Contacts/init_session`, promote, channel change), and all read/admin routes
stay unscoped so a poll can never consume a tenant's turn concurrency.

Limits live in a cluster-wide **rule book**, not on individual scopes. A rule
matches either an exact scope key or the wildcard `*`. The `*` rule is
per-scope, not shared: it gives **every** distinct scope key its own counter at
that limit, so `tenant-a` and `tenant-b` each get an independent budget. MOA
seeds one default rule, `* → concurrency 1000`, via restate-cli
(`restate rules set "*" --concurrency 1000 --description "scope default"`, backed
by the admin API); the local compose stack runs it from the one-shot
`restate-rules-bootstrap` service after Restate is healthy. Tighter per-tenant
caps are added later as exact-scope rules without code changes.

Enabling vqueues has a fresh-cluster limitation: a node accepts
`RESTATE_EXPERIMENTAL_ENABLE_VQUEUES=true` only when it has no in-flight
invocations, so the flag must be set before first traffic (wipe the
`moa-restate-data` volume when flipping it on an existing local node). Admission
state is observable through the Restate SQL introspection tables: `sys_rules`
lists the active rule book, and `sys_user_limits` reports per-scope counters and
current usage.

## Handler Surfaces

Current orchestrator surfaces are bound by one `moa-orchestrator` production
binary at startup. Domain logic behind those handlers should stay in-process
behind application services, repositories, or domain crates.

Core production bindings:

| Primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO` |
| Workflow | `TurnExecution`, `WorkerTurnExecution`, `ExecutionRun`, `ExecutionTask`, `KnowledgeSyncIngestion`, `KnowledgeIndexRebuild`, `Consolidate`, `ExperimentRun`, `ExperimentTrialRun` |
| Service | `ActionReviews`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `Eval`, `Execution`, `Experiments`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SecurityEvents`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy` |

Feature-gated bindings:

| Feature | Additional bindings |
|---|---|

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
| Current worker turn progress | `WorkerTurnExecution` workflow |
| Execution goal, plans, provenance, budget, completion, aggregate counters | Postgres `moa.execution_run` |
| Execution task state, generations, reservations, usage, citations, outputs | Postgres `moa.execution_task` |
| Pending tenant action reviews | Postgres `tenant_action_reviews` rows |
| Detached worker result waiters | `Worker` VO, resolved by child terminal delivery |
| Child heartbeat, last turn summary, pending input requests | `Worker` VO state (`last_heartbeat_at`, `last_turn_summary`, `pending_input_requests`, `cleanup_generation`) |
| Unread child signals, resume budget, pending resume | `Session` VO state (`unread_child_signals`, `resume_budget`, `pending_parent_resume_signal`, `resume_turn`) |
| Narration scheduling cursor and per-window cap | `Session` VO state (`narration_tick_generation`, `narration_tick_outstanding`, `narration_seq`, `last_narrated_marker`, `narration_window_*`) |
| Child liveness watchdog generations | `Session` VO state (`child_liveness`, `child_liveness_generation`) |
| Prompt-injection circuit for the coordinator turn | `Session` VO state (`security_circuit`) |
| Prompt-injection circuit for worker turns | `Worker` VO state (`security_circuit`) |
| Prompt-injection circuit for one execution task turn | `ExecutionTask` workflow, journaled. Deliberately not merged into the Session VO: a shared circuit alternating owners would let a detached task's generation switch clear a tripped coordinator's score. See `docs/02-brain-orchestration.md`. |
| Signed prompt-injection Detection Findings | Postgres `security_events`, written synchronously by the `SecurityEvents` service |
| Tool result and assistant output | Postgres event log |
| Graph memory, vectors, changelog | Postgres |
| Learning log | Postgres |
| Security events | Postgres |
| Hand leases and sandbox binding | Postgres `moa.hand_leases`, keyed `(session_id, worker_id, provider)` |
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

## Main-Agent/Worker Coordination In Inline Execute

Coordinator turns can return while detached workers keep running across
Kubernetes replicas. Coordination is split into two planes so the high-frequency
path never serializes through the single-writer parent VO.

This section describes conversational delegation in Inline Execute. `Worker` remains
available for interactive, steerable child-agent work but is not a plan node or
bulk DAG primitive. Worker fan-out controls do not cap execution-run maps.

**Telemetry plane (high-frequency, off the `Session` VO).**

- Child progress flows to the parent session event log through
  `turn_progress` exactly as before (`ProgressUpdate`).
- Each child's heartbeat updates `Worker` VO state only
  (`last_heartbeat_at`); no event is appended per tick. An event is appended
  only on a stale transition.
- `Session/progress` reads child summaries by **fan-in on demand**
  (`child_progress`), calling `Worker/progress_summary` only for active
  children under the existing fan-out cap — never absorbing every tick into
  parent VO state and never iterating the whole tree per request.
- A single **per-session narrator** emits one merged `ProgressNarrated` per
  period covering all active workers (plus the active coordinator step), at
  O(1) LLM cost regardless of fan-out (see Narration tick below).

**Control plane (low-frequency, through the coordinator VO).**

- A narrow child→parent attention signal (`ChildSignalKind` =
  `Finding`/`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale`) is routed to the
  owning root `Session` coordinator via `parent_session` and recorded by
  `Session::record_child_signal`.
- Recording is idempotent via a `dedupe_key` on the session event append
  (`worker_signal:{signal_id}`), updates the compact `unread_child_signals`
  projection, and may start **one** guarded coordinator resume turn when the
  parent is idle.
- Terminal completion keeps its existing path
  (`WorkerStatusChanged`/`WorkerNotificationDelivered` + cached result +
  result-waiter awakeable) and additionally records a control-plane idle-wake.
- `RunWorkerTurnRequest` carries a required `parent_session`, populated from
  `WorkerVoState` at dispatch. It is the only source of the owning session for a
  worker turn: the workflow never infers a missing parent, and a request without
  one is a typed decode error. Because it arrives on the request rather than being
  learned from the first prepared iteration, a worker turn that fails before that
  iteration can still append its parent-session facts.
- A failed worker turn appends the canonical `TurnFailed` fact (actor
  `Worker { worker_id }`) to the parent session before its `Failed` attention
  signal and before the owner callback, so the failure survives losing either.
  The attention signal and the worker-lifecycle events coexist with it and are
  neither substitutes for it nor duplicates of it.
- Only this plane writes parent VO state per event, and it fires rarely.

Postgres owns the replayable signal/session event history. Restate awakeables
back active result waits and the `needs_input` round-trip, not all delivery.
Redis is never a correctness owner for signals, resume, or terminal results.

### Scheduled VO ticks

The new autonomous behaviors run as Restate **delayed self-calls** off the VO,
through one shared helper `vo::schedule_generation_guarded_self_call` (modeled on
the `CronJob` delayed self-tick). Each schedules a single outstanding tick guarded
by a generation counter so superseded ticks no-op:

- **Narration tick** — `Session::narration_tick` (scheduled by
  `objects/session/narration.rs`). The tick never calls the model inline; when its
  gate opens (interval elapsed, change cursor moved, under the per-window cap) it
  `.send()`s a **detached** job `LLMGateway::narrate_session`, which reads the
  fan-in summaries, makes one cheapest-chat-model call (or short-circuits with no
  call when only one source is active), and appends one `ProgressNarrated` with
  dedupe key `narration:{session_id}:{narration_seq}`. Default-on
  (`progress_narration_enabled`).
- **Liveness watchdog** — `Session::check_child_liveness`
  (`objects/session/liveness.rs`), armed per active child. On a stale heartbeat it
  appends `WorkerHeartbeatStale` (dedupe key
  `worker_stale:{worker_id}:{last_heartbeat_at_ms}`) and raises a
  `HeartbeatStale` control signal. A child parked on a `needs_input` request is
  exempt (`awaiting_input`), so it is never flagged stale while legitimately
  waiting.
- **Self-cleanup** — `Worker::cleanup`, a generation-guarded delayed self-call
  scheduled after the child reports terminal. It releases the child's own sandbox,
  removes itself from the parent's fan-out, and clears VO state; a follow-up that
  arrives during the grace window revives the child instead.

### Guarded parent resume

An idle coordinator can start at most one bounded turn per resume-eligible signal.
The resume is fenced by an active-turn gate (a signal stays queued in
`unread_child_signals` while a root turn is active), signal-id dedupe
(`pending_parent_resume_signal`), and a per-session `resume_budget`
(`worker_resume_max_per_window` over `worker_resume_window_ms`; default 6 per
10 minutes). Resume is conservative: it fires only on
`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale` with `ParentResumePolicy::IfIdle`,
never on `progress`/`heartbeat`/`finding`/plain success. The resume turn runs as
the session's already-recorded owning identity (no broad authz bypass).
`record_child_signal` records `WorkerParentResumeRequested` and dispatches the
turn with `RunTurnRequest.trigger = ChildSignal`; that turn branch skips the
synthetic `UserMessage` append (`user_message` instead carries the
system-generated resume instruction), and the history pipeline renders the
already-recorded resume event. A running coordinator turn drains its dispatch-time
snapshot of queued signals at context-compile time so a `NeedsInput`/`Blocked`
child is not stranded.

### Cancellation scope

`Session::cancel` takes a `CancelScope`: `TaskTree` (default) cancels the active
coordinator turn and the whole recursive child tree (today's behavior);
`CoordinatorOnly` cancels only the active `TurnExecution` and leaves children
running. The dead `Soft`/`Hard` `CancelMode` is removed.

## Execution Run Coordination

`ExecutionRun` is the only durable graph controller. It loads the persisted
goal contract and active canonical plan, asks the pure `moa-execution`
interpreter for all ready logical work, materializes stable task rows, and sends
every ready `ExecutionTask` invocation durably. It advances only from persisted
typed outcomes. There is no application active-worker, plan-node, or
execution-task concurrency constant. The run's approved `max_tasks` and other
resource dimensions bound logical work; Restate scoped concurrency and provider
pacing queue physical execution.

The graph is acyclic and has exactly seven operations: `Capability`, `Agent`,
`Map`, `Reduce`, `Review`, `WaitSignal`, and `Output`. A map creates one task for
each stable item key and cannot contain another map. Reduce uses structured
batches; an agent reducer is a deterministic hierarchical tree bounded by
`batch_size`.

Before dispatch, `ExecutionTask` atomically reserves worst-case microusd,
tokens, tasks, tool calls, retrieved bytes, and deadline allowance. A task that
cannot reserve does not start. On completion it reconciles actual integer usage
and writes output/citations through the current generation fence. Retry and
recovery can therefore neither double-spend nor let stale completion overwrite
new work.

Task-local agents may use declared instruction-only skills and governed
capabilities for a bounded number of turns. They return only `Completed`,
`NeedsInput`, `NeedsReplan`, or `Failed`. `NeedsInput` parks the exact run/task.
`NeedsReplan` asks the planner for a structured amendment using the immutable
goal, active plan, completed outputs, evidence, remaining budget, and current
catalog. The compiler rejects changes to running/completed work, cycles,
recursive maps, task-identity reuse with new semantics, excess budget, and
authorization expansion. Accepted amendments increment `plan_revision` and
append canonical patch/hash/reason records. Repeated hashes or failure
fingerprints, no progress, deadline, or resource exhaustion terminate with
exact partial/blocked coverage; there is no arbitrary amendment-count cap.

Cancellation prevents new reservations and leaves completed task rows
queryable. Terminal completion requires every immutable goal requirement,
deliverable, coverage item, schema/citation check, and budget/deadline check to
pass. The run writes compact aggregate output, citations, failures, and gaps,
emits terminal session events, and requests at most one synthesis turn. Raw map
outputs stay in execution persistence, not session history or Session VO state.

## Determinism Rules

Code inside Restate handlers must keep replay safety in mind:

- External side effects go through Restate service calls, workflows, or
  journaled `ctx.run` sections.
- Time, randomness, and generated IDs must use deterministic Restate helpers or
  be produced inside journaled blocks.
- Retried handlers must be idempotent or guarded by product-level idempotency
  keys.
- Prefer a *derived* identity over a journaled generated one where the fact has
  natural coordinates. A prompt-injection circuit transition keys its session
  fact and its OCSF finding off a digest of the transition's own coordinates and
  a UUIDv5 over that digest, so a replay reproduces both without depending on
  the journal replaying an id in the same order. Time still has to be journaled:
  the owner reads the clock once inside a named `ctx.run` and passes that value
  to everything downstream, so nothing re-reads it on a later attempt.
- Do not perform direct network or filesystem side effects in replay-sensitive
  sections unless they are journaled.

## Action Reviews

Action reviews do not suspend root or worker workflows. The turn workflow
stores the review request in Postgres, appends a session event, returns a
pending-review tool result to the model, and continues:

```text
tool call requires admin review
  -> workflow stores tenant action review
  -> the review is registered on its typed owner (Session or Worker VO)
  -> action-review event is persisted
  -> pending-review tool result is appended
  -> tenant admin decides later through ActionReviews
  -> after the decision and the cleared tool's terminal event are durable, the
     owner receives one typed receipt and runs one continuation turn
```

The continuation fact `ActionReviewContinuationRequested` is deduped on
`action_review_continuation:{review_id}`, so replay neither appends it twice nor
dispatches a second continuation turn. Owner generations and continuation
scheduling live in Restate VO state as a derived index; the authoritative facts
remain the `tenant_action_reviews` row plus the durable `ActionReviewDecided` and
terminal `ToolResult`/`ToolError` events. An `ExecutionTask` owner is excluded
from this path and keeps its run/task/generation outbox and ack contract.

Gateway processes never own pending review state. If a gateway restarts, it can
reconstruct pending tenant action reviews from Postgres.

Worker tool calls use the same action-review path as root turns. A pending
tenant-admin review records product state in Postgres and returns a
pending-review tool result to the child turn; it does not create a blocked
worker awakeable.

## Cancellation

MOA supports both:

| Path | Use |
|---|---|
| Cooperative cancellation | User asks the session or turn to stop; workflow checks at deterministic boundaries and records a normal cancelled outcome. |
| Restate invocation cancellation | Operator hard-stops a stuck invocation through Restate admin APIs. |

`Execution/cancel` is the product cancellation path for a run. It fences new
task reservations, durably records the run terminal/partial state, and preserves
completed task evidence before terminal session delivery.

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

VO state persists until explicitly cleared. Session and worker state should
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

Graceful shutdown flips readiness false, waits five seconds for load-balancer
drain, then cancels and joins Restate, SCIM, channel, and background-job
producers. It drains lineage before audit and flushes metrics before traces.
Individual joins and drains are bounded at 15 seconds, so Restate can reassign
work that does not finish.

## Observability

Handler spans should include Restate identity plus MOA tenant/session
attributes. The useful diagnostic chain is:

1. Restate invocation id and handler.
2. Session id, turn id, tenant id.
3. Postgres session events.
4. OTel trace and span links.
5. Provider/tool timing and retry counters.

Execution spans add run ID, task ID, plan hash/revision, requirement IDs,
reservation/actual usage, retry generation, capability reference, and terminal
reason as trace fields rather than high-cardinality metric labels. Operators
diagnose a run from `moa.execution_run`/`moa.execution_task`, Restate
invocations, traces, and its compact session events.

Dashboards should separate Restate health, turn latency, LLM/provider behavior,
approval latency, tool execution, and sandbox fleet health.

## Local Development

Local development uses the same Restate-backed orchestrator as cloud mode.
Bring the compose stack up only when the task needs Postgres, Restate, OpenFGA,
edge, opt-in PII, or load-test services. Stop it with
`docker compose down` when finished unless the task explicitly requires keeping
services up.

`docs/02-brain-orchestration.md` describes the current boot sequence and the
turn flow implemented by `Session` plus `TurnExecution`.
Worker conversational state is held by `Worker`; each admitted child turn
runs in `WorkerTurnExecution`, and detached waits use child-owned result
awakeables plus parent-cached terminal results instead of status polling.

## Current Decisions

1. Postgres is the system of record; Restate is the orchestration engine.
2. Sessions and workers are virtual objects.
3. Top-level turns run in `TurnExecution` workflows keyed by turn ID.
4. Worker turns run in `WorkerTurnExecution` workflows keyed by turn ID.
5. `ExecutionRun` and `ExecutionTask` are the only durable typed-DAG runtime;
   `Worker` remains conversational delegation in Inline Execute.
6. Tenant action reviews use the `ActionReviews` service plus Postgres rows
   and events; they do not block turn workflows.
7. Product-visible events, execution state, learning, memory, lineage, and audit stay in
   Postgres.
8. Gateways and clients can always rebuild visible state from Postgres records.
