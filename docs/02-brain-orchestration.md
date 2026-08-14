# 02 — Brain Orchestration

_Restate orchestration, hosted API runtime mode, turn execution, and workers._

## Source Of Truth

`docs/12-restate-architecture.md` is the detailed Restate architecture document. This file summarizes what the current code runs:

- Cloud runtime: `moa-orchestrator`
- Client surface: HTTP routes on `moa-edge` and Restate ingress test calls
- Shared turn helpers: `crates/moa-orchestrator/src/turn/`
- Session VO: `crates/moa-orchestrator/src/objects/session/`
- Worker VO: `crates/moa-orchestrator/src/objects/worker/`
- Turn workflows: `crates/moa-orchestrator/src/workflows/turn_execution/mod.rs` and `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- Execution domain: `crates/moa-execution/`
- Execution persistence/activation contracts: `crates/moa-execution/src/repository/` and `crates/moa-orchestrator/src/runtime/endpoint.rs`
- CronJob VO: `crates/moa-orchestrator/src/objects/cron_job.rs`
- Pipeline assembly: `crates/moa-brain/src/pipeline/mod.rs`
- Sandbox workspace contract: `docs/25-sandbox-workspaces.md`

## Cloud Runtime

`moa-orchestrator` is the single production binary and HTTP handler service
registered with Restate. At startup it:

1. Loads shared `MoaConfig` from flat `MOA_...` environment variables.
2. Connects with the runtime Postgres URL and validates that the exact complete
   central migration history is already installed.
3. Builds one `RuntimeDeps` dependency graph containing the Postgres stores,
   graph memory stack, provider registry, runtime cache, tool router, connector
   services, shared `IngestRuntime`, and shared `MemoryRetrievalEngine`.
4. Injects concrete turn, authorization, credential, delivery, retrieval, and
   ingestion dependencies into their handler implementations.
5. Binds Restate services, virtual objects, and workflows.
6. Starts the Restate endpoint and a separate health/readiness endpoint.

Normal replicas never receive Restate Admin API authority. Kubernetes
RestateDeployment Operator owns registration and version retention. A distinct
revisioned bootstrap Job observes registration, dispatches one-way state
migrations, and reconciles default CronJob virtual objects.

There is no process-global orchestrator context or ingestion-runtime singleton.
`RuntimeDeps` is the sole production composition root, and `build_endpoint`
only binds the already-constructed graph.

Schema changes are an explicit deployment phase. Only
`moa-orchestrator migrate` uses `MOA_DATABASE_ADMIN_URL` and executes migration
DDL; the default runtime command never falls back to migration authority and
fails closed before dependency construction when history is missing or drifts.
Kubernetes runs that command in an init container before starting each runtime
replica.

Core production Restate bindings:

| Restate primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO`, `ExecutionRunController`, fleet-keyed `ExecutionDispatchDrain` |
| Service | `ActionReviews`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `DurableTimeout`, `Execution`, `ExecutionDispatcher`, `ExecutionDispatchReconciler`, `ExecutionRetention`, `ExecutionSchedule`, `ExecutionTrigger`, `Experiments`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy` |
| Workflow | `KnowledgeSyncIngestion`, `Consolidate`, `ExecutionTaskAttempt`, `ExecutionCompensationAttempt`, `SkillLearning`, `TurnExecution`, `WorkerTurnExecution`, `ExperimentRun`, `ExperimentTrialRun` |

Internal application boundaries for action reviews, builtin async-authz
challenges, learning review, experiments, privacy, provider routing, and memory
retrieval are in-process boundaries behind the handlers above. Read-only
analytics, whoami, audit verification, and lineage explain/query/verify are
direct edge handlers over Postgres/domain stores. These boundaries are
extraction seams inside the monolith, not a direction to create internal
network services.

Restate state is used for hot orchestration state: queued messages, status,
child refs, active segment, pending cancellation scope, awakeables, and child
budgets.
Product-visible history is written to Postgres. Kubernetes traffic is
non-sticky, so correctness state shared across incoming requests must live in
Postgres, Restate, or the orchestrator's required Redis-compatible Valkey
runtime cache; process memory is only a local cache.
Arbitrary files created inside a sandbox are not Restate or session state.
They become durable only after the `SandboxWorkspace` commit barrier publishes a
verified portable checkpoint; see [Sandbox Workspaces](25-sandbox-workspaces.md).

`Artifacts` owns import, export, listing, validation, and publish for canonical
skills, connectors, actions, and agents. `moa-execution` owns execution-plan
compilation, pure scheduling, budgets, completion, and run/task persistence.
The `Execution` service exposes start, status, list, cancel, pause/resume,
review, signal, callback, and bounded result operations. Postgres owns durable
graph execution; `ExecutionRunController`, `ExecutionTaskAttempt`,
`ExecutionCompensationAttempt`, `ExecutionTrigger`, the head-coalescing `ExecutionDispatcher`,
`ExecutionDispatchDrain`, and
`ExecutionDispatchReconciler` are bounded activations over that state.
`ExecutionSchedule` and `DurableTimeout` own bounded schedule/timeout mutations;
`ExecutionRetention` owns bounded terminal-detail archival and deletion. The
open-ended agent loop remains in `Session` and `TurnExecution`.

## Session Flow

```text
client sends message
  -> SessionStore creates/loads session metadata
  -> Session::set_meta initializes VO state when needed
  -> Session::start_turn consults the admission fence,
     then records an active turn id
  -> Session sends TurnExecution::run keyed by turn_id
  -> TurnExecution appends the message, runs the brain loop, and records events
  -> TurnExecution calls back to Session::record_turn_outcome
  -> Session dispatches the next queued message in FIFO order, if any
```

`Session::record_turn_outcome` owns queue disposition. The handler-owned
`SessionPendingState` is the only pending-message projection; the queue lives in
`pending_messages` and is dispatched one message at a time, oldest first.
Completed, accepted, **failed**, and coordinator-only-cancelled outcomes all
dispatch the next queued message: stopping on failure would strand acknowledged
messages behind a turn that never comes back.

A callback for a turn that is no longer active is a complete no-op. It cannot
rewrite `last_outcome` or the session summary, and it cannot touch a newer active
turn; only waiters keyed to that turn resolve. This runs before any validation,
so a duplicate delivery never becomes a retryable error.

Cancellation scope decides the rest. `CoordinatorOnly` stops one turn and the
queue continues. `TaskTree` appends one `QueuedMessageRejected` fact per
already-accepted queued message in FIFO order, drains the queue immediately, and
fences admission: `start_turn` returns a typed 409
until the cancelled turn reports its outcome, so a message that raced the
cancellation cannot start a turn inside a tree being torn down. The scope is
recorded against the turn it cancels and released only by that turn's matching
outcome, and no queued work is dispatched after a task-tree cancelled callback.
The former write-only `SessionVoState.cancel_flag` is gone; it was never read and
was not a fence.

`Session::start_turn` is the only message-submitting handler. It is serialized
by Restate's single-writer-per-key semantics but stays fast:
the VO mutates K/V state and sends a durable workflow invocation. The
long-running LLM/tool loop lives in `TurnExecution`, so concurrent `snapshot`,
`start_turn`, and `request_cancel` calls do not wait behind a running turn.
There is no previous session-local turn runner; `TurnExecution` owns the durable
turn loop.

`TurnExecution` first selects one public route, then owns its turn mechanics.
Ordinary user language is classified by at most one strict, no-tools auxiliary-model
call before route execution. Therefore Respond means one user-facing response
call after at most one classifier call, not one total provider call. Trusted
template invocation, blank-objective preflight, and internal synthesis bypass
the classifier. A typed Durable upgrade is a separate trusted control
transition and is never classifier input. Any uncertain or malformed classifier
result selects Execute/Inline without retry or planner fallback.

- Respond makes one model call with no tools and no planning call.
- Execute carries exactly one explicit strategy. Inline runs the bounded
  root model/tool loop and may use conversational workers. Durable instantiates
  a pinned skill template or compiles a strict generated plan, persists it,
  admits a run and dispatches `ExecutionRunController` detached, then returns acceptance without polling it
  from the root model.
- NeedsInput appends one deterministic clarification carrying bounded missing
  fields.

Skills are optional inputs after routing, never routes or admission gates.
Custom instruction-only skills work in the Inline loop and in declared Durable
`Agent` nodes; their absence is not an execution error.

An initial root Execute/Inline turn may upgrade exactly once to Durable after
discovering high-fan-out, resumable, approval-bearing, or otherwise durable
work. It preserves bounded evidence and the byte-identical objective, does not
call the classifier again, and cannot downgrade. Task difficulty is not a
Durable signal.

Upgrade authority belongs to the workflow-owned `request_durable_execution`
control tool. It is injected only for the eligible root Inline turn, must be
called alone, and carries the bounded rationale and evidence into the one-way
transition. Arbitrary tool-result payloads cannot trigger an upgrade.

1. Build a `CompletionRequest` from session events and the context pipeline.
2. Ensure a task segment exists or roll to a new segment when query rewrite marks `is_new_task`.
3. Select Respond, Execute, or NeedsInput through trusted control facts or one
   bounded auxiliary classifier call; require an explicit Inline/Durable
   strategy for Execute. A bounded explanatory rationale may live in the active
   turn, but it is neither interpreted as control data nor persisted in route
   audits or analytics.
4. Persist assistant output and tool calls.
5. Build an `ActionEnvelope`, evaluate action policy, and route allowed tool execution through `ToolExecutor`.
6. Record tool usage, skill activation, token usage, and turn counts on the active segment.
7. Apply turn outcome and update session status.
8. Assess idle, cancelled, or completed segments and append `learning_log` entries.
9. Derive experience records, attributions, and proposed learning candidates after assessment persistence.
10. Dispatch a detached `SkillLearning` workflow after experience persistence succeeds.

The turn loop is durable because external calls and side effects are wrapped through Restate handlers or `ctx.run()` boundaries. Cancellation is delivered through a workflow promise; the workflow checks it at deterministic boundaries and races it against the in-flight LLM call. Awakeables are used for builtin async-authz challenges and worker result waits, not for tool action review or turn cancellation. Skill-learning proposal generation is intentionally detached: turn completion does not wait for a draft skill proposal, and generation failures are recorded as warning events rather than turn failures.

### Lineage Sink Selection

`MOA_LINEAGE_SINK` controls how the cloud orchestrator emits lineage events:

- unset / `null`: disables the lineage sink. This is the production default
  unless a deployment explicitly enables Postgres lineage storage.
- `otel`: emits span attributes only.
- `postgres`: commits accepted events to `analytics.lineage_journal` before
  returning, then claims and writes them to `analytics.turn_lineage` and
  related lineage tables. Acceptance is a Postgres commit, so a record survives
  the replica that accepted it and any replica can finish it; there is no local
  path to configure.

The durable sink runs an in-memory queue (`MpscSink`) and a background writer
that drains and replays the durable journal. Queue pressure can drop only
explicitly configured lossy telemetry; audit-class events are not accepted
before the journal append succeeds. Maximum queue depth and batch size come
from `config.observability.lineage` in `MoaConfig`; every duration and capacity
knob must be greater than zero. The optional `[clickhouse]` configuration is
analytics-only and never changes lineage storage or reads.

`TurnExecution` threads its workflow key through context compilation as the
lineage `turn_id`. Graph-memory retrieval, compiled context, generation,
citation, and online score records for the same user turn use that id so
direct lineage explain reads can render one turn tree. The compiled-context record
also carries structured source references for event-log messages, tool
messages, and graph-memory nodes when those sources are known.

### Provider Overrides For Test Runs

`MOA_PROVIDERS_OVERRIDE` is a dev/CI-only startup switch for replacing normal
LLM providers inside `moa-orchestrator`. It is available only in binaries built
with the `moa-orchestrator/provider-overrides` feature:

- unset: use providers configured from normal API keys.
- `scripted:<path>`: use a JSON fixture with deterministic responses.
- `mock:<seed>`: use the built-in deterministic mock response.

The orchestrator refuses to start with an override when the environment is
`prod` or `production`, and a default build also refuses overrides because the
scripted provider is not compiled in. The checked-in load-test fixture lives at
`crates/moa-loadtest/scripts/perf-gate.json`; see `docs/20-testing.md` for the
script format.

## Action Policy

Tool calls are checked at the tool boundary. Auto mode defaults to `Allow`, while persisted rules and config can return `Allow`, `Deny`, or `AdminReview`.

```text
Tool call
  -> refuse if the security circuit disabled this capability for the owner
  -> build ActionEnvelope (carrying exactly one typed ActionReviewOwner)
  -> evaluate ActionPolicies
  -> Allow: execute ToolExecutor
  -> Deny: record ToolError and continue
  -> AdminReview: register the review on its owner, persist the tenant action
     review, return pending-review tool result, continue
```

The circuit check precedes policy evaluation: a capability the circuit already
disabled never reaches the policy engine or the executor, and the model receives
a fixed safe notice for the call it made.

Tenant action reviews are decided by tenant admins through `ActionReviews`; conversation clients do not unblock turns.

### Action-review owners and continuations

Every `ActionEnvelope` carries exactly one `ActionReviewOwner`: `Coordinator`
(session, turn, generation), `Worker` (session, worker, turn, generation), or
`ExecutionTask` (session, run/task/generation). Ownership is decided by the
runtime that issued the tool call and is never inferred later.

`ActionReviews/request` registers a conversational review on its owner
synchronously, before the pending-review tool result reaches the model. A worker
with an unresolved current-generation review is not terminal: it does not resolve
parent waiters, emit its terminal report, schedule cleanup, or discard local
history until the review resolves or is superseded.

When a review resolves, the owner receives one typed receipt
(`ClearedSuccess`, `ClearedToolError`, or `Denied`) carrying the review, both tool
ids, the owner, and closed-vocabulary outcome metadata. Tool/admin output remains
in canonical history and is never copied into the system directive. The callback
is sent only after `ActionReviewDecided` and, for a cleared action, the executed
tool's terminal `ToolResult`/`ToolError` are durable. The reviewed execution is a
new MOA-owned invocation: it gets a fresh tool-call id and drops the provider
tool-use id.

The owner then runs one continuation turn:

- Coordinator: `TurnTrigger::ActionReview` — no classifier, no planner, no tools,
  no durable upgrade, one bounded `Respond` call, at most one visible answer.
- Worker: one no-tools synthesis turn that updates local history and result, then
  normal parent-result and cleanup ownership resumes.
- Execution task owner: no conversational callback at all; it stays on the
  durable task-generation outbox and acknowledgement path.

Review timeout remains fail-closed and produces no conversational resume. The
reaper durably releases the Session or Worker lifecycle hold through a
claim/backoff delivery record, so an expired review cannot pin its owner.

Continuations are generation-fenced. A callback arriving while the origin or
another continuation is active queues once and runs before ordinary FIFO, unless a
newer user message (or worker follow-up) advanced the generation, which strands it.
An unresolved review never blocks a later user message. Multiple reviews continue
in durable registration order, one follow-up per review.

## Prompt-Injection Security Circuit

`docs/08-security.md` owns the detection policy — the classifier, the typed
classes, and the additive score. What belongs here is where the circuit sits in
the durable machinery.

**Classification is inside the journal.** The `ToolExecutor` classifies within
its own `ctx.run` closure, so Restate journals the safe output and its
assessment together. Classifying after the closure returned would journal raw
bytes and re-derive the assessment on every replay.

**Scoring is owned by whoever can make it atomic.** The Session VO owns the
coordinator's circuit and the Worker VO owns each worker turn's; both expose an
internal atomic apply handler that performs the whole read-score-write and
returns the exact transition, so two tool results landing in the same turn
cannot interleave into a lost update.

An execution task instead scores against circuit state held by its own persisted
task generation and loaded by the bounded attempt. Do not "simplify" this into
the Session VO alongside the other two.
A single shared circuit alternates owners as work moves between the coordinator
and detached tasks, and adopting a new owner generation clears the capability
map — so an attacker who has tripped the coordinator's circuit could reset it
just by causing any detached task to run. Per-task circuits under per-task
owners remove that move entirely. The cheaper arguments point the same way: a
task attempt is a single sequential writer, so there is no interleaving to
defend against, and generation-fenced Postgres state survives retries without a
VO round-trip per scored output. The owner is
generation-fenced — `Coordinator { turn_id, generation }`,
`Worker { worker_id, turn_id, generation }`, or
`ExecutionTask { run_uid, task_uid, generation }` — and state resets only for a
genuinely new owner generation, never for a new input fingerprint, new tool
arguments, a fallback Hand provider, or an activation replay. A delayed
action-review continuation runs under a new dispatch id but keeps the original
logical owner, which is why the owner travels in the request rather than being
inferred from the caller.

**Audit precedes effect.** When a stage boundary is crossed the owner journals
one timestamp, appends the neutral `PromptInjectionCircuitTransition` keyed by
the transition digest, then makes a *synchronous* call to the internal
`SecurityEvents` service before applying its outcome. A halt must never take
effect with no audit record explaining why. The service is separate from the VO
so the single-writer object never blocks its queue on a Postgres write, while
the caller still awaits durability.

**Owner outcomes are exact.** A coordinator suspend registers a
generation-fenced coordinator-input reply target on the Session, releases its
fleet/tenant turn-admission lease, and idles indefinitely until that reply arrives.
The exact authenticated reply reacquires admission before resolving the awakeable;
a coordinator halt records the canonical actor+turn
`TurnFailed`. A worker suspend emits one `NeedsInput` signal with
`input_audience: User` and awaits its awakeable on the existing worker-input
machinery; a worker halt emits one `Failed` signal and terminates that worker
turn. An execution task returns `ExecutionTaskResult::NeedsInput { audience:
User }` or `ExecutionTaskResult::Failed { class: Terminal }`. Worker and task
circuit facts stay neutral in the shared session log; their own signals and task
outcomes own the suspension or termination.

## Workers

`Worker` is a Restate virtual object because delegated work can be conversational. It stores:

- owning root session
- depth
- budget remaining and tokens used
- task and tool subset
- pending messages and local history
- result waiters and cancellation reason
- last turn summary, last heartbeat, and the generation for its one outstanding
  liveness deadline
- pending `needs_input` requests and the self-cleanup generation counter
  (`pending_input_requests`, `cleanup_generation`)

`Worker` admits conversational messages and starts at most one `WorkerTurnExecution` workflow per active child turn. Workflow callbacks carry the admitted `turn_id`; stale responses, tool results, approval clears, and outcomes are ignored rather than mutating a newer turn.

Delegation is owned by the root coordinator in Execute/Inline. Workers are bounded
general-purpose child agents: they run task-local turns with scoped tools and
budgets, report results, and do not own decomposition or final synthesis.
Workers do not spawn or manage other workers. Coordinator delegation is bounded
by active fan-out, repeated active task detection, and inherited token budgets.
`spawn_worker` returns immediately, and `wait_worker` first consumes any cached
terminal worker result; otherwise it registers a worker-owned result waiter
awakeable and removes that waiter on timeout. Terminal worker results are cached
on the session until consumed so finished detached workers free active fan-out
without losing the final result. `spawn_worker.task` carries the purpose,
relevant context, expected output, evidence requirements, constraints, and
relevant skill instructions; optional controls bound tools, tokens, and turns.

Workers support interactive, steerable delegation inside Execute/Inline. They are not
plan nodes, map items, reducers, or the bulk DAG substrate. Work that needs an
explicit dependency graph, durable joins, scalable map materialization, review
waits, or exact coverage uses stable execution run and task rows plus bounded
controller/attempt activations.
Conversational worker fan-out limits are separate from the
`execution.max_in_flight_tasks` physical window that bounds live DAG tasks.

### Event-driven coordination

Coordinator turns can return while detached children keep running. Coordination
keeps high-frequency progress off the single-writer parent VO and gives each
state transition one durable owner. `docs/12-restate-architecture.md` is the
detailed reference.

- Turn progress is cadence-limited and delivered directly by
  `turn_progress::maybe_emit`; it does not schedule Session work. Heartbeats
  update `Worker` state only and move the effective liveness deadline without
  scheduling a second overlapping timer.
- Model-authored `Finding`, `Blocked`, and `NeedsInput` use the awaited
  `Session::record_child_signal` path. A Worker's exact stale deadline sends
  `HeartbeatStale` through the same joined path. `Finding` is recorded but is
  never resume-eligible.
- Terminal state has one parent-side owner. The Worker durably records its
  terminal result and resolves explicit result awakeables before awaiting
  `Session::record_worker_child_terminal`. The Session handler validates the
  registered worker and admission generation, caches the result, claim-checks
  large output, and appends `WorkerStatusChanged` plus
  `WorkerNotificationDelivered` idempotently.
- A failed terminal produces exactly one `Failed` attention signal and guarded
  resume. Successful or cancelled terminals produce no per-child wake. Child
  registration advances the fan-in generation; when its last registered child
  settles successfully or is cancelled while the coordinator is idle, Session
  records at most one `FanInSettled` signal for that generation and starts one
  guarded resume. When the coordinator is active, terminal facts stay cached
  for its explicit waiter/list path and no automatic success wake is queued.

`post_message` / `WorkerMessage::FollowUp` remains the parent→child primitive.
There is no command bus and no second message queue: steering, the `needs_input`
answer, and revival all reuse the existing message path.

### Guarded parent resume and `needs_input`

An idle coordinator can start at most one bounded turn per resume-eligible signal,
fenced by an active-turn gate, signal-id dedupe (`pending_parent_resume_signal`),
and a per-session resume budget (`worker_resume_max_per_window` over
`worker_resume_window_ms`). Resume is conservative — only
`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale`/`FanInSettled` with
`ParentResumePolicy::IfIdle`, never `Finding`, progress, or an individual plain
success — and runs as the session's recorded owning identity. `FanInSettled` is
valid only for the current child-registration generation and is recorded at
most once after every child in that generation is terminal. The resume turn
carries `RunTurnRequest.trigger = ChildSignal`, which skips the synthetic user
message; the brain renders the recorded `WorkerParentResumeRequested` as a
system directive. A running coordinator turn also drains queued signals at
context-compile time, so unread `NeedsInput`/`Blocked` signals reach the model
without an additional wake.

`needs_input` is a child→parent round-trip on the same message path: the child's
`request_input` tool registers a Restate awakeable, emits a `NeedsInput` signal
carrying `input_request_id`/`input_audience`, checkpoints and releases any worker
sandbox compute, and blocks indefinitely on the durable awakeable. The coordinator
answers with the
`provide_worker_input` tool → `WorkerMessage::ProvideInput`, which resolves
the awakeable through `post_message`. Coordinator-audience questions are answered
autonomously. User-audience questions are exposed as `worker_input_request` SSE
frames; the next plain user reply is forwarded by the session to the worker as
`WorkerMessage::ProvideInput` instead of starting a separate root turn. Restate owns
the suspended wait without an active handler or provider call; the worker's next sandbox
dispatch restores the exact portable checkpoint onto fresh compute.

### Self-cleanup and the liveness watchdog

After the joined terminal acknowledgement, the Worker marks parent notification
delivered and schedules a generation-guarded delayed `Worker::cleanup` self-call
after `worker_cleanup_grace_ms`. Cleanup checkpoints according to workspace
policy, releases the child's ephemeral compute attachment, retains the durable
workspace or reconciliation owner independently, removes the child from the
parent's fan-out, and clears VO state. A follow-up that arrives during the grace
window revives the child instead, and messages to a cleaned/terminal child are
rejected rather than re-bootstrapped.

Worker also owns one liveness deadline. Its first accepted task or heartbeat
arms one delayed self-call. Later heartbeats update `last_heartbeat_at` but do
not add timers. When the call fires, it stops for terminal or `awaiting_input`
state, reschedules once for the exact latest-heartbeat deadline when still
fresh, or sends one joined `HeartbeatStale` signal to Session and stops when
genuinely stale. Session never polls Worker state to discover staleness.

## Workflows And Bounded Execution Activations

Restate workflows run internal durable jobs:

- `Consolidate`: one tenant/date memory consolidation pass.
- `KnowledgeSyncIngestion`: one tenant knowledge sync ingestion pass.
- `TurnExecution`: one durable session turn keyed by `turn_id`; runs the top-level session brain loop and calls back to `Session` on completion, cancellation, or failure.
- `WorkerTurnExecution`: one admitted worker turn keyed by `turn_id`; runs child-local LLM/tool loops and calls back to `Worker` with turn-scoped mutations.

Long-horizon execution is deliberately not workflow-shaped. A run may remain
nonterminal for seconds through weeks, but no Restate invocation spans that
lifetime. Postgres stores the immutable admitted `Identity`, plan, scheduler
projection, run and task generations, attempt leases, compensation stack,
waits, triggers, external jobs, capacity reservations, and dispatch outbox.

`ExecutionRunController/advance` serializes activations by `run_uid`, claims one
persisted wake epoch, performs at most `maximum_activation_steps`, dispatches at
most `dispatch_batch_size` ready tasks, commits progress, and returns.
`ExecutionTaskAttempt/run` and the `ExecutionCompensationAttempt` slice each
execute one immutable dispatch generation and return after outcome, retry,
review, signal, timer, pause, external-job start, or watchdog classification.
Every stale attempt loses its generation fence.

Input, review, signal, timer, pause, and external-job waits are storage-only.
Entering a wait resolves an `After { delay_seconds }` target from that exact
wait-entry instant or retains an explicit UTC `At { at }`, persists `due_at`,
releases active-attempt and hand capacity, and enqueues an immutable trigger.
Reusable templates accept only nonzero `After`; generated one-off plans may use
`At`. `ExecutionTrigger/fire` and the reconciliation owner redeliver the same
generation-fenced transition through the outbox, so no polling workflow is
needed.

Task dispatch atomically reserves tenant and fleet active-attempt capacity plus
the task's worst-case integer cost, token, task, tool-call, retrieved-byte, and
deadline allowance. Parked-run, scheduled-trigger, and external-job ceilings
are independent durable capacity classes. Weighted tenant dispatch prevents a
single tenant from consuming the fleet queue.

A sandbox-using worker or execution task is also the only valid owner of a
`SandboxWorkspace`; a coordinator/bare session is not. Mutating sandbox tools
cannot publish a successful `ToolResult` until the workspace commit barrier has
quiesced the writer and advanced a verified immutable portable checkpoint.
Sandbox dispatch without a typed worker or execution-task workspace scope is
rejected before workspace reads or provider I/O.

The plan is an acyclic graph with exactly `Capability`, `Agent`, `Map`,
`Reduce`, `Review`, `WaitSignal`, `WaitUntil`, and `Output`. A map task is only a capability
or agent and cannot nest another map. Agent tasks can use declared
instruction-only skills and capabilities with bounded turns and budgets. They
cannot mutate the graph. Unexpected conditions return typed `NeedsInput` or
`NeedsReplan`; every amendment is compiled, authorization-narrowing, budgeted,
persisted in `plan_history`, and applied only to pending or downstream work.
Each automatic amendment generation or repair call first reserves cost and
tokens from the run ledger; unavailable capacity stops before gateway dispatch,
while completed calls reconcile exact provider usage and retain per-call audit
attribution under replay.
Repeated hashes, recurring failure fingerprints, no progress, deadline, or
resource exhaustion terminate with exact partial/blocked coverage instead of an
infinite loop.

Cancellation first fences new reservations and cancels active attempts.
The plan's explicit policy then either retains already committed effects or
enters `Compensating` and invokes atomically registered compensators in reverse
commit order. Compensation uses stable identities and generation fencing, so a
restart does not repeat a completed undo. Ambiguous or exhausted undo settles
with `manual_repair_required`; it never reports a clean rollback. Completed
forward and compensation evidence remains queryable. A run cannot become
`completed` until every immutable goal-contract
requirement and completion check passes. Terminal state emits compact aggregate
output, citations, failures, and gaps to the owning session. The session starts
at most one deduplicated synthesis turn for the originating user sequence; it
does not ingest every raw map output or poll the run through the root model.

Public `confirm`, `cancel`, `pause`, `resume`, `deliver_input`, `decide_review`,
`deliver_signal`, external callback, and `apply_amendment` requests acknowledge
success only after the repository mutation and generation-fenced dispatch row
commit. If immediate delivery fails, the singleton maintenance owner reclaims
the outbox row; replay cannot repeat the logical transition.

Behavior-lab execution uses the same reserve-before-dispatch rule. An
`ExperimentRun` requires one exact immutable `experiment_plan` revision and
pages its trial coordinates through `PlanTrialPager`; there is no direct raw
target/variant/scorecard run path. Each `ExperimentTrialRun` reserves its
simulator and target coordinates separately.
Simulator coordinates are admitted only with an exact certified policy
revision; the workflow revalidates the stored immutable policy snapshot and
uses its provider/model, decoding, prompt, context, and structured response
contract for production gateway dispatch. Simulator decisions and the policy
binding enter terminal evidence. Execution-template targets do not use a
simulator coordinate.
The admitted target slice is carried through `Session` admission into
`TurnExecution`, `LLMGateway`, and `ToolExecutor`; loop, model, tool, and
deadline gates may narrow it but never widen it. Bounded turns cannot detach
durable execution, spawn workers, or enqueue an admin review until those child
paths can receive a non-duplicating sub-reservation.

The deterministic and sampled validation of these claims is defined in
[Execution Honesty Evaluation](eval/execution-honesty.md). Those checks consume
the same persisted projection, task rows, planning audits, and bounded session
event evidence as runtime inspection; they do not reconstruct success from a
prose transcript.

Reusable product execution schedules live in `moa.execution_schedule`. Each
occurrence has a stable schedule/occurrence identity, admitted owner `Identity`,
timezone-aware policy, misfire policy, concurrency policy, next occurrence, and
generation. The maintenance owner incrementally materializes immutable
`schedule_occurrence` triggers; start-run admission remains tenant/fleet
capacity-gated and idempotent. Stopping or reconfiguring a schedule advances a
monotonic incarnation tombstone, and every tick from an older incarnation is a
no-op.

Platform maintenance schedules remain anchored by the `CronJob` virtual object.
Each job key stores its cron expression, timezone, target service handler, and
the same monotonic incarnation rule; stop/reconfigure never resets the counter
or lets a late tick regain authority.

### Background Maintenance Jobs

On boot, the orchestrator installs eight periodic jobs via the `CronJob`
virtual object:

- `graph_memory_compact`: fires at HH:00 UTC every hour and invokes
  `GraphMemoryMaint/compact`, which queues one `Consolidate` workflow for each
  active graph-memory tenant.
- `vector_sync_outbox_drain`: fires every minute and invokes
  `GraphMemoryMaint/sync_vectors` with `limit = 512`.
- `segment_materialized_views_refresh`: fires every 15 minutes and invokes
  `SessionStore/refresh_segment_materialized_views`.
- `analytics_materialized_views_refresh`: fires every 15 minutes and invokes
  `SessionStore/refresh_analytics_materialized_views`.
- `skill_regression_monitor`: fires every 15 minutes and invokes
  `SessionStore/monitor_skill_regressions`.
- `task_recurrence_monitor`: fires every 15 minutes and invokes
  `SessionStore/mine_task_recurrences`.
- `learning_embeddings_backfill`: fires every 15 minutes and invokes
  `SessionStore/backfill_learning_embeddings`.
- `neon_prune_branches`: fires at 00:00, 06:00, 12:00, and 18:00 UTC and
  invokes `NeonMaint/prune_branches`. It is a no-op when `MOA_NEON_API_KEY` is
  unset.

To inspect the schedule:

```bash
curl http://localhost:10010/restate/call/CronJob/graph_memory_compact/status
```

To pause or resume a job without clearing its config:

```bash
curl -X POST http://localhost:10010/restate/call/CronJob/graph_memory_compact/pause
curl -X POST http://localhost:10010/restate/call/CronJob/graph_memory_compact/resume
```

To install a custom schedule, post a new body to
`/restate/call/CronJob/{key}/configure` and bump the bootstrap
idempotency-key version suffix in code.

## Hosted API Runtime

MOA ships no embedded command/runtime client. Local development starts
`moa-orchestrator` through `make dev`, and automation exercises that service
through `moa-edge` public routes or direct Restate ingress calls in tests.
Development and cloud execution therefore use the same orchestrator binary and
handler surface.

The local compose stack still uses:

- `PostgresSessionStore`
- graph memory store, ingestion, and hybrid retrieval stack
- the same context pipeline
- the same tool router and permission store
- the same draft skill proposal and review paths

Scheduling and recovery are Restate-managed in both local development and cloud
deployments.

## Segment And Learning Hooks

The orchestrator is responsible for connecting task work to learning:

- `SegmentStarted` and `SegmentCompleted` events are persisted in the event log.
- `task_segments` stores the current segment state and counters.
- Segment assessment writes `segment_assessed`.
- Experience extraction writes immutable `experience_records` from assessed segments.
- Attribution writes `experience_attributions` for skills, tools, memory, policy, and verification evidence.
- Candidate generation writes `learning_candidates` at the initial status their `proposal_kind` allows — `Proposed` only for the two reviewable kinds, `Advisory` or `NeedsAuthoring` for observations no code can materialize. Transitions are gated in the database, not only by convention.
- `SkillLearning` writes only draft skill artifacts and proposed skill candidates.
- `LearningReview` is the only runtime service that activates accepted skill drafts, appends `skill_created` or `skill_improved`, and marks the candidate promoted.
- Rejected skill candidates preserve draft artifacts for audit and never activate skill revisions.

This makes the learning pipeline event-sourced enough to audit and rollback without hiding updates inside model prompts.
