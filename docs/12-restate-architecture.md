# 12 - Restate Architecture

_Durable execution on Restate services, virtual objects, and workflows._

## Purpose

Restate is MOA's durable execution engine. Postgres remains the product record
for sessions, events, memory, analytics, learning, lineage, and audit. Restate
owns orchestration state: queues, bounded activation delivery, awakeables,
retries, and handler journals. Postgres is the recovery authority for
long-horizon execution; a run never depends on one invocation surviving for its
product lifetime.

Restate does not own arbitrary sandbox filesystem bytes and cannot make them
durable merely because a session or workflow is durable. Sandbox compute is
ephemeral; `SandboxWorkspace` rows and portable checkpoint bytes have separate
Postgres and provider/object-store owners. Restate journals the authorized,
generation-fenced lifecycle calls described in
[Sandbox Workspaces](25-sandbox-workspaces.md).

This document defines how MOA maps product concepts to Restate primitives and
what must stay out of Restate state.

## Primitive Selection

| Restate primitive | Use in MOA | Reason |
|---|---|---|
| Service | Durable calls such as `ActionReviews`, `AuthzChallenges`, `DurableTimeout`, `Execution`, `ExecutionDispatcher`, `ExecutionDispatchReconciler`, `ExecutionRetention`, `ExecutionSchedule`, `ExecutionTrigger`, `LearningReview`, `ToolExecutor`, `LLMGateway`, `SecurityEvents`, `SessionStore`, `Authz`, `Memory`, `Skills`, `Tenants` | Durable bounded RPC. |
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO`, `ExecutionRunController`, fleet-keyed `ExecutionDispatchDrain` | Single-writer-per-key semantics and small hot state; run controllers serialize per-run activation while the drain serializes admission against fleet-global capacity. Producer kicks enter the stateless `ExecutionDispatcher`, which coalesces them by the exact indexed outbox head. |
| Workflow | `TurnExecution`, `WorkerTurnExecution`, `ExecutionTaskAttempt`, `ExecutionCompensationAttempt`, `KnowledgeSyncIngestion`, `Consolidate`, `ExperimentRun`, `ExperimentTrialRun` | One bounded logical job per ID with explicit progress and completion. |

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
| Execution run | Postgres aggregate plus keyed `ExecutionRunController` activations | `run_uid` |
| Execution task | Postgres row plus bounded `ExecutionTaskAttempt` activation | stable hash of `(run_uid, node_id, item_key)` and generation |
| Execution compensation | Postgres stack plus bounded compensation-attempt slice | stable compensation registration and dispatch IDs |
| Execution timer/deadline/schedule | Immutable Postgres trigger plus `ExecutionTrigger` delivery | `trigger_uid` and generation |
| Sandbox workspace lifecycle | `moa-hands` application/repository boundary called from Restate handlers/workflows, plus Postgres rows | `workspace_id` with writer and instance generations |
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
because one admitted turn should have one observable bounded run. Typed graph
work instead uses Postgres state plus short controller, attempt, and trigger
activations so waits do not pin handlers or deployment revisions. Tenant
knowledge sync ingestion and consolidation are workflows for the same reason.
Knowledge index rebuild/rechunk and the hosted tenant `Eval` service are not
runtime surfaces. Regression evals run through the platform-only
`moa-eval`/`xtask` harness; live behavior trials use the `Experiments` service
and its workflows.

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
`start_turn` is the only message-submitting handler and passes
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
immutable goal contract and canonical plan, dispatches `ExecutionRunController` detached, and
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

MOA has one execution-flow-control owner: the `Session` virtual object's
`TurnAdmission` policy backed by the required Redis-compatible Valkey runtime
cache. It acquires both a
fleet lease and a tenant lease before dispatching a coordinator turn. The
session ID is the stable lease identity, so replay and heartbeat renewal are
idempotent across replicas. Limits, lease TTL, and retry cadence come from the
typed `session_limits.turn_admission_*` configuration.

When either shared limit is full, the `Session` handler journals the Valkey
lease attempt with `ctx.run`, durably sleeps for the configured retry interval,
and tries again. Capacity saturation is a wait state, not a terminal 429 and
not a second queue. Restate still serializes requests for the same Session, so
an active session retains its bounded persisted pending-message queue while
other tenants continue on independent Session keys. `Session/progress` uses a
`SharedObjectContext`, so progress reads remain responsive while an exclusive
turn-start handler is waiting for capacity.

The lease spans the real turn lifetime. A generation-fenced heartbeat renews
it while that turn remains active, and every matching terminal path releases
both fleet and tenant leases before the next queued turn starts. The TTL only
reclaims leases after a crashed owner stops heartbeating. No second ingress or
cluster-level flow-control layer participates in MOA's admission model.

## Handler Surfaces

Current orchestrator surfaces are bound by one `moa-orchestrator` production
binary at startup. Domain logic behind those handlers should stay in-process
behind application services, repositories, or domain crates.

Core production bindings:

| Primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO`, `ExecutionRunController`, fleet-keyed `ExecutionDispatchDrain` |
| Workflow | `TurnExecution`, `WorkerTurnExecution`, `ExecutionTaskAttempt`, `ExecutionCompensationAttempt`, `KnowledgeSyncIngestion`, `Consolidate`, `ExperimentRun`, `ExperimentTrialRun` |
| Service | `ActionReviews`, `ActionReviewDispatcher`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `DurableTimeout`, `Execution`, `ExecutionDispatcher`, `ExecutionDispatchReconciler`, `ExecutionRetention`, `ExecutionSchedule`, `ExecutionTrigger`, `Experiments`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SecurityEvents`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy` |

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
| Execution admitted identity, goal, plans, provenance, budget, completion, activation state, aggregate counters | Postgres `moa.execution_run` |
| Execution node/task/attempt/compensation state, generations, waits, reservations, usage, citations, outputs | Postgres `moa.execution_node_state`, `moa.execution_task`, and `moa.execution_compensation` |
| Execution deadlines, timers, wait expiry, watchdogs, schedules, external jobs, capacity, and activation delivery | Postgres trigger/schedule/external-job/capacity/dispatch-outbox tables |
| Pending tenant action reviews | Postgres `tenant_action_reviews` rows |
| Detached worker result waiters | `Worker` VO, resolved by child terminal delivery |
| Child heartbeat, one outstanding liveness deadline, last turn summary, pending input requests | `Worker` VO state (`last_heartbeat_at`, liveness generation/outstanding state, `last_turn_summary`, `pending_input_requests`, `cleanup_generation`) |
| Unread child signals, current fan-in generation/settlement, resume budget, pending resume | `Session` VO state (`unread_child_signals`, registered child generations, settled generation, `resume_budget`, `pending_parent_resume_signal`, `resume_turn`) |
| Prompt-injection circuit for the coordinator turn | `Session` VO state (`security_circuit`) |
| Prompt-injection circuit for worker turns | `Worker` VO state (`security_circuit`) |
| Prompt-injection circuit for one execution task generation | Postgres task-generation state loaded by its bounded attempt. Deliberately not merged into the Session VO: a shared circuit alternating owners would let a detached task's generation switch clear a tripped coordinator's score. See `docs/02-brain-orchestration.md`. |
| Signed prompt-injection Detection Findings | Postgres `security_events`, written synchronously by the `SecurityEvents` service |
| Tool result and assistant output | Postgres event log |
| Graph memory, vectors, changelog | Postgres |
| Learning log | Postgres |
| Security events | Postgres |
| Ephemeral hand leases and compute binding | Postgres `moa.hand_leases`, attached to a typed worker/execution-task workspace and fenced independently from workspace revision |
| Workspace ownership, lifecycle, writer/instance fences, checkpoint head, operations, grants, capacity, retention, delete fence | Postgres `moa.sandbox_workspace*` rows owned by `moa-hands` repositories |
| Active mutable workspace bytes | Selected provider working storage; never the committed revision |
| Portable committed checkpoint bytes | Durable S3-compatible object storage; references/digests in Postgres, bytes outside Restate/session state |
| Runtime cache/pacing/message refs | Required Redis-compatible Valkey; process-local memory is limited to isolated non-orchestrator tests |
| Handler journal | Restate |

If a user, admin, customer, or audit export needs to query it later, store it
in Postgres. If only the in-flight handler needs it to recover, Restate state
is appropriate.

The only workspace owners are `Worker { session_id, worker_id }` and
`ExecutionTask { run_id, task_id }`. A coordinator or bare session cannot admit
one. Sandbox dispatch enforces that contract before workspace reads or provider
I/O. `moa-orchestrator` owns authentication, authorization, and the Restate
durability boundary around workspace calls; `moa-hands` owns the lifecycle,
repositories, operation/capacity ledgers, checkpoint logic, and provider
adapters.

Kubernetes request routing is non-sticky. A follow-up request may land on any
edge or orchestrator replica, so Restate handler state cannot be replaced by
ordinary process memory. Process-local maps are allowed only as reconnect
caches, transport demultiplexing, or performance caches whose correctness owner
is Postgres, Restate, or the required Redis-compatible Valkey runtime cache.

## Coordination Advancement Matrix

These are the complete work-discovery and resume sources for the three durable
coordination owners:

| Owner advanced | Allowed source | Persist-before-wake and acknowledgement contract |
|---|---|---|
| `Session` | Accepted user/reply admission or a matching queued-turn outcome | The Session admission/queue fence is journaled before dispatching the exact `TurnExecution`. |
| `Session` | Model-authored `Blocked` or `NeedsInput`, or a Worker's exact stale deadline | `Session::record_child_signal` appends the idempotent fact before any guarded resume and acknowledges the caller. `Finding` is recorded but never wakes. |
| `Session` | Worker terminal transition | Worker persists terminal state and resolves result waiters, then awaits the sole Session terminal handler. The handler records exactly one failure consequence or, when the last registered child settles successfully or is cancelled, at most one `FanInSettled` consequence for that generation before acknowledgement. |
| `Session` | Execution-run terminal synthesis request | The compact terminal run projection and synthesis dedupe are durable before the guarded synthesis turn is dispatched. |
| `Worker` | Accepted follow-up/input, an admitted turn callback, or its own exact liveness/cleanup deadline | Worker state and generation fences select the exact continuation; no Session status read discovers Worker work. |
| Execution run | A committed run/task/trigger/callback mutation and its dispatch-outbox row | `ExecutionRunController` claims the exact wake generation. Immediate delivery may fail because maintenance redelivers the persisted row. |

Input/review expiry, run deadlines, task timers/watchdogs, external reconcile,
and schedule occurrences are immutable execution triggers. Conversation lease
heartbeats and platform maintenance schedules remain separate safety
mechanisms. No owner advances from a repeating status read, recursive progress
call, or elapsed-time task scan.

## Main-Agent/Worker Coordination In Inline Execute

Coordinator turns can return while detached workers keep running across
Kubernetes replicas. Cadence-limited progress remains off the single-writer
parent VO, while every parent-affecting transition has one durable owner.

This section describes conversational delegation in Inline Execute. `Worker` remains
available for interactive, steerable child-agent work but is not a plan node or
bulk DAG primitive. Worker fan-out controls are separate from the
`execution.max_in_flight_tasks` physical window for execution-run maps.

**Direct progress and attention signals.**

- Child progress is cadence-limited by `turn_progress::maybe_emit` and delivered
  directly as `ProgressUpdate`; it never schedules Session coordination work.
- Model-authored `Finding`, `Blocked`, and `NeedsInput` are routed through the
  awaited `Session::record_child_signal` path. Recording is idempotent via
  `worker_signal:{signal_id}` and updates `unread_child_signals`. `Finding` is
  never resume-eligible.
- Each heartbeat updates only `Worker.last_heartbeat_at`. The Worker owns one
  outstanding delayed liveness call and sends `HeartbeatStale` through the
  joined Session signal path only when that exact deadline is stale.
- `RunWorkerTurnRequest` carries a required `parent_session`, populated from
  `WorkerVoState` at dispatch. It is the only source of the owning session for a
  worker turn: the workflow never infers a missing parent, and a request without
  one is a typed decode error. Because it arrives on the request rather than being
  learned from the first prepared iteration, a worker turn that fails before that
  iteration can still append its parent-session facts.

**One terminal handoff owner.**

- Worker persists its terminal state locally and immediately resolves every
  explicit `wait_worker` awakeable. It then makes one joined call to
  `Session::record_worker_child_terminal` with the worker id, admission
  generation, terminal result, journaled signal id, and journaled timestamp.
- That Session handler validates the registered child and generation, caches and
  claim-checks the terminal result, and appends `WorkerStatusChanged` plus
  `WorkerNotificationDelivered` idempotently. It is the only parent-facing
  terminal signal writer.
- A failed terminal emits exactly one `Failed` attention signal and guarded
  resume. Successful and cancelled children do not wake separately. Registering
  a child advances the Session fan-in generation; when the last child in that
  generation settles successfully or is cancelled while the coordinator is idle,
  the handler emits at most one `FanInSettled` signal and guarded resume. If the
  coordinator is active, terminal facts remain cached for its explicit
  waiter/list path and no automatic success wake is queued.
- After Session acknowledges, Worker marks notification delivered and schedules
  cleanup. `wait_worker` still completes from the cached result or child-owned
  awakeable and never schedules another parent resume.

Postgres owns the replayable signal/session event history. Restate awakeables
back active result waits and the `needs_input` round-trip, not all delivery.
Redis is never a correctness owner for signals, resume, or terminal results.

### Exact Worker deadlines

Worker uses Restate delayed self-calls for two exact deadlines:

- **Liveness** — the first accepted task or heartbeat arms one generation-fenced
  delayed Worker call. Later heartbeats update `last_heartbeat_at` without
  scheduling another overlapping call. When it fires, terminal or
  `awaiting_input` state stops it; a fresh heartbeat reschedules once for the
  exact latest deadline; a genuinely stale heartbeat appends one
  `WorkerHeartbeatStale`, sends one joined `HeartbeatStale` signal, and stops.
- **Self-cleanup** — `Worker::cleanup`, a generation-guarded delayed self-call
  scheduled after the child reports terminal. It checkpoints according to
  policy, releases the child's ephemeral compute attachment, retains the durable
  workspace/reconciliation owner independently, removes itself from the
  parent's fan-out, and clears VO state; a follow-up that arrives during the
  grace window revives the child instead.

### Guarded parent resume

An idle coordinator can start at most one bounded turn per resume-eligible signal.
The resume is fenced by an active-turn gate (a signal stays queued in
`unread_child_signals` while a root turn is active), signal-id dedupe
(`pending_parent_resume_signal`), and a per-session `resume_budget`
(`worker_resume_max_per_window` over `worker_resume_window_ms`; default 6 per
10 minutes). Resume is conservative: it fires only on
`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale`/`FanInSettled` with
`ParentResumePolicy::IfIdle`, never on progress, heartbeat, `Finding`, or an
individual plain success. `FanInSettled` is valid only after every child in the
current registration generation is terminal and can be recorded once for that
generation. The resume turn runs as the session's already-recorded owning
identity (no broad authz bypass).
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

## Long-Horizon Execution Coordination

Postgres is the only durable graph controller state. Admission persists the
complete `Identity`, immutable goal/plan, absolute run deadline, controller
generation, and initial dispatch row in one transaction. Every subsequent
controller, task, compensation, trigger, callback, or operator mutation loads
that identity; no activation re-derives authority from ambient request state.
Pending and waiting rows are storage-only.

`ExecutionRunController/advance` is keyed by `run_uid`. One activation claims a
persisted wake epoch, applies at most `maximum_activation_steps`, dispatches at
most `dispatch_batch_size` stable ready rows, records aggregate progress, and
returns. `ExecutionTaskAttempt/run` executes one task generation within the
active-attempt timeout. Compensation uses one bounded
`ExecutionCompensationAttempt` slice per immutable dispatch identity. No
activation sleeps until a product event or retains an attached child for the
run lifetime.

Public confirm/cancel/pause/resume/input/review/signal/callback/amendment
handlers commit their generation-fenced transition and transactional outbox row
before returning. Immediate Restate delivery is an optimization; the singleton
maintenance owner reclaims undelivered rows and due triggers. The stable
dispatch/trigger identity makes duplicate delivery a no-op.

The graph is acyclic and has exactly eight operations: `Capability`, `Agent`,
`Map`, `Reduce`, `Review`, `WaitSignal`, `WaitUntil`, and `Output`. A map creates one task for
each stable item key and cannot contain another map. Reduce uses structured
batches; an agent reducer is a deterministic hierarchical tree bounded by
`batch_size`.

`Review`, `WaitSignal`, run input, and `WaitUntil` carry explicit expiry or wake
targets. `At { at }` denotes an exact UTC instant and is valid for a generated
one-off plan. Nonzero `After { delay_seconds }` is resolved from the instant the
task enters the wait. Reusable templates reject `At` and use `After`, so earlier
dependency duration cannot make a template timer stale. Entering any wait
persists `due_at`, releases attempt and hand capacity, and schedules an immutable
trigger; expiry follows `FailTask`, `FailTask`, or `ContinueWith { output }`.

Before dispatch, the repository atomically reserves worst-case microusd,
tokens, tasks, tool calls, retrieved bytes, deadline allowance, and tenant/fleet
active-attempt capacity. Active runs, active attempts, parked runs, scheduled
triggers, and external jobs have distinct tenant/fleet ceilings. Weighted
tenant dispatch controls fleet fairness. A task that cannot reserve starts no
work, and retry/recovery cannot double-spend or let a stale completion overwrite
new work.

Task-local agents may use declared instruction-only skills and governed
capabilities for a bounded number of turns. They return a terminal result, a
typed wait, a bounded replan request, or an asynchronous external-job start.
External jobs persist provider/job identity, generation, callback disposition,
and sparse reconciliation before the attempt returns; callbacks cannot bypass
the outbox or generation fence. A typed wait parks the exact run/task.
`NeedsReplan` asks the planner for a structured amendment using the immutable
goal, active plan, completed outputs, evidence, remaining budget, and current
catalog. The compiler rejects changes to running/completed work, cycles,
recursive maps, task-identity reuse with new semantics, excess budget, and
authorization expansion. Accepted amendments increment `plan_revision` and
append canonical patch/hash/reason records. Repeated hashes or failure
fingerprints, no progress, deadline, or resource exhaustion terminate with
exact partial/blocked coverage; there is no arbitrary amendment-count cap.

Cancellation fences new reservations and advances active attempt generations.
`retain_effects` preserves committed work; `compensate_committed` moves the run
to `Compensating` and dispatches registered compensators in strict reverse
commit order. Each bounded slice either commits an outcome, schedules a retry or
review, pauses, or records an ambiguous result as `manual_repair_required`.
Completed undo is never repeated.

Every root-turn, worker-turn, and execution-attempt model call carries its typed
owner to `LLMGateway`; the gateway removes that metadata before provider
dispatch and observes the shared hashed cancellation fence around active
provider I/O. Postgres, not Valkey, decides the task generation and terminal
state. Terminal completion requires every immutable goal requirement,
deliverable, coverage item, schema/citation check, and budget/deadline check to
pass. Compact aggregate output enters session history; raw task and
compensation evidence remains in execution persistence.

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
- Journaling a sandbox command does not journal its filesystem effects. A
  `MayWrite` call becomes successful only after the `moa-hands` commit barrier
  quiesces the writer, publishes/verifies an immutable portable checkpoint, and
  compare-and-set advances the workspace head. Ambiguous outcomes remain
  `reconciling` and do not fall back to an empty provider.

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
terminal `ToolResult`/`ToolError` events. An execution-task owner is excluded
from this conversational path and keeps its persisted task/generation outbox
and acknowledgement contract.
Timed-out Session and Worker reviews use a separate durable release-only
delivery on the review row; it removes the lifecycle hold but cannot schedule a
continuation.

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
task reservations and advances active-attempt generations before applying the plan's
explicit `retain_effects` or `compensate_committed` policy. Only after retained
effects or reverse-order compensation reaches a durable settled state does the
run publish terminal session delivery; all evidence is preserved.

Prefer cooperative cancellation for product flows because it preserves normal
events, cleanup, and hand teardown.

## Failure And Idempotency

Restate retries failed invocations according to handler configuration. Tool
calls still need product-level idempotency:

LLM completion is the costly exception to generic handler retry. Provider
adapters own three retries (four HTTP attempts) for each configured failover
candidate, while the enclosing `LLMGateway` journaled run permits exactly one
Restate attempt. The documented upper bound for one logical completion is
therefore `configured failover candidates * 4`; Restate never multiplies it.
Every paid workflow call also supplies a versioned Restate idempotency key from
the caller invocation ID plus its typed action coordinate and attempt or turn
ordinal. Restate therefore retains the completed gateway result independently
of the parent journal boundary, so a parent crash cannot redispatch that action.

- read-only tools may retry freely;
- remote writes should use idempotency keys when available;
- non-idempotent calls should not be retried after a side effect is confirmed.

After retry exhaustion, operators should inspect the Restate invocation, trace,
and Postgres session events before resuming or cancelling. Do not hide repeated
handler failure behind unbounded application-level retry loops.

Every binding declares the Restate retry baseline in v4 discovery: 50ms initial
delay, factor 2, 60-second maximum delay, 70 total attempts, then `PAUSE` for
operator inspection. Typed terminal errors bypass that retry sequence. The
provider adapters remain the only owner of repeated paid HTTP attempts; the
`LLMGateway` journaled provider run itself is single-attempt, so handler replay
reuses its durable result rather than multiplying provider calls.

`LLMGateway`, `ToolExecutor`, `TurnExecution`, `WorkerTurnExecution`,
`ExecutionRunController`, `ExecutionTaskAttempt`, `ExecutionCompensationAttempt`,
`ExecutionTrigger`, `ExecutionDispatcher`, fleet-keyed `ExecutionDispatchDrain`,
`ExecutionDispatchReconciler`, `ExecutionRetention`, and `DurableTimeout` are
ingress-private. `ExecutionSchedule` remains the tenant-authorized schedule
mutation surface. The private handlers are reachable only service-to-service;
public traffic enters through edge-owned product surfaces. Their inactivity
timeout is 360 seconds with a 60-second abort cleanup window. Provider stream
configuration is capped at 300 seconds, leaving that cleanup margin. Durable
human waits suspend and therefore do not need a larger
inactivity timeout. The normal product endpoint does not contain
`Session/migrate_status_idle` or `StatusMigrationDispatcher`. Those two handlers
exist only in the pre-runtime migration endpoint; the raw Session handler is
ingress-private, and the endpoint intentionally omits `Health` and every product
turn handler so it cannot open edge admission.

The singleton maintenance process owns low-frequency action-review
reconciliation and gauge sampling. Normal expiry is one generation-fenced
`DurableTimeout` delivery scheduled when the review is created. Execution-task
resolution commits the task-generation transition and execution dispatch row;
no lifetime task workflow is resumed.

## Journal And Retention

Journals are for bounded activation recovery and recent debugging, not
long-term product history.
All bindings explicitly retain idempotency entries and journals for 24 hours,
preserving the current effective behavior without depending on a mutable server
default. Every workflow `run` handler likewise declares 24-hour completion
retention. Postgres remains the long-term product and audit record.

VO state persists until explicitly cleared. Session and worker state should
be cleared after the product session is terminal and old enough that live
recovery is no longer needed.

## Deployment

`moa-orchestrator` is a normal HTTP handler service registered with Restate.
Production should run Restate as a durable cluster, keep the handler endpoint
internal, and expose public traffic through `moa-edge`.

The versioned serving role binds Restate, SCIM, channel, and credential
ingress. The stable singleton `moa-orchestrator maintenance` role binds only
health/metrics and owns trigger/outbox reconciliation, retention,
action-review/authz reconciliation, workspace/hand reaping, and provider
inventory. Serving revisions do not duplicate those loops. Because all product
waits are storage-only, an old serving revision retains only bounded
invocations; the RestateDeployment operator may autoscale a draining revision
to one recovery replica and then zero when its invocation count reaches zero.

Deployment requirements:

- Postgres/Neon for product data.
- Restate ingress URL for runtime invocation. Normal replicas have no Admin API
  configuration or network grant; Operator owns registration and version
  retention, while the revisioned bootstrap Job receives its Admin URL as an
  explicit command argument.
- Redis-compatible Valkey for Session turn admission, pacing, and shared runtime
  cache coordination. Orchestrator startup fails if this backend is absent or
  resolves to process-local memory.
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
reason as trace fields rather than high-cardinality metric labels. Fleet
metrics aggregate phase, oldest-ready/deadline/trigger/outbox/attempt/external
age, capacity/fairness, durable maintenance last-success age, and draining revision cost.
Operators diagnose a run from Postgres execution state, its current bounded
Restate activation, traces, and compact session events.

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
5. Postgres execution aggregates plus bounded `ExecutionRunController`,
   `ExecutionTaskAttempt`, `ExecutionCompensationAttempt`, `ExecutionTrigger`,
   `ExecutionDispatcher`, fleet-keyed `ExecutionDispatchDrain`, and
   `ExecutionDispatchReconciler` activations are the only durable typed-DAG
   runtime. The singleton `fleet` drain key serializes bounded outbox delivery
   and admission against fleet-global capacity; `ExecutionSchedule` and
   `DurableTimeout` provide schedule/timeout delivery, `ExecutionRetention` owns
   bounded terminal-detail archival and deletion, while `Worker` remains
   conversational delegation in Inline Execute.
6. Tenant action reviews use the `ActionReviews` service plus Postgres rows
   and events; they do not block turn workflows.
7. Product-visible events, execution state, learning, memory, lineage, and audit stay in
   Postgres.
8. Gateways and clients can always rebuild visible state from Postgres records.
9. Sandbox compute is ephemeral. Durable filesystem state belongs to a
   worker-owned or execution-task-owned `SandboxWorkspace`; provider/object
   storage owns bytes, while Restate owns only its replayable lifecycle calls.
