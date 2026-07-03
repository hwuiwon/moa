# 02 — Brain Orchestration

_Restate orchestration, hosted API runtime mode, turn execution, and workers._

## Source Of Truth

`docs/12-restate-architecture.md` is the detailed Restate architecture document. This file summarizes what the current code runs:

- Cloud runtime: `moa-orchestrator`
- Client surface: HTTP routes on `moa-edge` and Restate ingress test calls
- Shared turn helpers: `crates/moa-orchestrator/src/turn/`
- Session VO: `crates/moa-orchestrator/src/objects/session/`
- Worker VO: `crates/moa-orchestrator/src/objects/worker/`
- Turn workflows: `crates/moa-orchestrator/src/workflows/turn_execution.rs` and `crates/moa-orchestrator/src/workflows/worker_turn_execution.rs`
- Procedure execution: `crates/moa-orchestrator/src/workflows/procedure_execution.rs`
- CronJob VO: `crates/moa-orchestrator/src/objects/cron_job.rs`
- Pipeline assembly: `crates/moa-brain/src/pipeline/mod.rs`

## Cloud Runtime

`moa-orchestrator` is the single production binary and HTTP handler service
registered with Restate. At startup it:

1. Loads shared `MoaConfig` from flat `MOA_...` environment variables.
2. Connects to Postgres and runs session migrations.
3. Builds the Postgres session store, graph memory stack, provider registry,
   embedding provider, runtime cache, and tool router.
4. Installs an `OrchestratorCtx` singleton for handlers.
5. Binds Restate services, virtual objects, and workflows.
6. Starts the Restate endpoint and a separate health/readiness endpoint.

Core production Restate bindings:

| Restate primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `Worker`, `Tenant`, `CronJob`, `IngestionVO` |
| Service | `ActionReviews`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy` |
| Workflow | `ProcedureExecution`, `KnowledgeSyncIngestion`, `Consolidate`, `TurnExecution`, `WorkerTurnExecution` |

Feature-gated Restate bindings:

| Feature | Additional bindings |
|---|---|
| `experiments` | `Experiments` service plus `ExperimentRun` and `ExperimentTrialRun` workflows |
| `internal-eval-runner` | `Eval` service |
| `skill-learning` | `SkillLearning` workflow |

Internal application boundaries for action reviews, builtin async-authz
challenges, learning review, experiments, privacy, provider routing, and memory
retrieval are in-process boundaries behind the handlers above. Read-only
analytics, whoami, audit verification, and lineage explain/query/verify are
direct edge handlers over Postgres/domain stores. These boundaries are
extraction seams inside the monolith, not a direction to create internal
network services.

Restate state is used for hot orchestration state: queued messages, status,
child refs, active segment, cancellation flags, awakeables, and child budgets.
Product-visible history is written to Postgres. Kubernetes traffic is
non-sticky, so correctness state shared across incoming requests must live in
Postgres, Restate, or an explicitly configured Redis runtime cache; process
memory is only a local cache.

`Artifacts` owns import, export, listing, validation, and publish for canonical
skills, connectors, actions, and agents. The `Skills` service exposes the skill
procedure run lifecycle over Restate, while `ProcedureExecution` executes the
deterministic procedure graph using the pure interpreter in `moa-skills`.
The open-ended agent loop still lives in `Session` and `TurnExecution`.

Procedure runs can carry an optional `session_id` so the product can show a
procedure attached to the same support conversation. This is an association
boundary, not autonomous routing: skill selection still happens inside the
context pipeline, and procedure node execution remains explicit procedure
runtime behavior.

## Session Flow

```text
client sends message
  -> SessionStore creates/loads session metadata
  -> Session::set_meta initializes VO state when needed
  -> Session::post_message / Session::start_turn records an active turn id
  -> Session sends TurnExecution::run keyed by turn_id
  -> TurnExecution appends the message, runs the brain loop, and records events
  -> TurnExecution calls back to Session::record_turn_outcome
  -> Session drains the next queued message, if any
```

`Session::post_message` and the explicit `Session::start_turn` path are
serialized by Restate's single-writer-per-key semantics, but they stay fast:
the VO mutates K/V state and sends a durable workflow invocation. The
long-running LLM/tool loop lives in `TurnExecution`, so concurrent `snapshot`,
`queue_message`, and `request_cancel` calls do not wait behind a running turn.
There is no previous session-local turn runner; `TurnExecution` owns the durable
turn loop.

`TurnExecution` owns the turn mechanics:

1. Build a `CompletionRequest` from session events and the context pipeline.
2. Ensure a task segment exists or roll to a new segment when query rewrite marks `is_new_task`.
3. Call `LLMGateway`.
4. Persist assistant output and tool calls.
5. Build an `ActionEnvelope`, evaluate action policy, and route allowed tool execution through `ToolExecutor`.
6. Record tool usage, skill activation, token usage, and turn counts on the active segment.
7. Apply turn outcome and update session status.
8. Assess idle, cancelled, or completed segments and append `learning_log` entries.
9. Derive experience records, attributions, and proposed learning candidates after assessment persistence.
10. When skill learning is compiled, dispatch a detached `SkillLearning` workflow after experience persistence succeeds.

The turn loop is durable because external calls and side effects are wrapped through Restate handlers or `ctx.run()` boundaries. Cancellation is delivered through a workflow promise; the workflow checks it at deterministic boundaries and races it against the in-flight LLM call. Awakeables are used for builtin async-authz challenges and worker result waits, not for tool action review or turn cancellation. Skill-learning proposal generation is intentionally detached: turn completion does not wait for a draft skill proposal, and generation failures are recorded as warning events rather than turn failures.

### Lineage Sink Selection

`MOA_LINEAGE_SINK` controls how the cloud orchestrator emits lineage events:

- unset / `null`: disables the lineage sink. This is the production default
  unless a deployment explicitly enables Postgres lineage storage.
- `otel`: emits span attributes only.
- `postgres`: journals accepted events to the configured fjall path before
  queueing them, then writes to `analytics.turn_lineage` and related lineage
  tables in the same Postgres database the orchestrator already uses. In cloud,
  the journal path must be an explicit persistent mounted path, not pod-local
  temp storage.

The Postgres sink runs an in-memory queue (`MpscSink`) and a background writer
that drains and replays the durable journal. Queue pressure can drop only
explicitly configured lossy telemetry; audit-class events are not accepted
before the journal append succeeds. Maximum queue depth and batch size come
from `config.observability.lineage` in `MoaConfig`.

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
  -> build ActionEnvelope
  -> evaluate ActionPolicies
  -> Allow: execute ToolExecutor
  -> Deny: record ToolError and continue
  -> AdminReview: persist tenant action review, return pending-review tool result, continue
```

Tenant action reviews are decided by tenant admins through `ActionReviews`; conversation clients do not unblock turns.

## Workers

`Worker` is a Restate virtual object because delegated work can be conversational. It stores:

- owning root session
- depth
- budget remaining and tokens used
- task and tool subset
- pending messages and local history
- result waiters and cancellation reason
- last turn summary and last heartbeat for the telemetry plane (`last_turn_summary`, `last_heartbeat_at`)
- pending `needs_input` requests and the self-cleanup generation counter (`pending_input_requests`, `cleanup_generation`)

`Worker` admits conversational messages and starts at most one `WorkerTurnExecution` workflow per active child turn. Workflow callbacks carry the admitted `turn_id`; stale responses, tool results, approval clears, and outcomes are ignored rather than mutating a newer turn.

Delegation is owned by the root coordinator. Workers do not spawn or manage
other workers. Coordinator delegation is bounded by active fan-out, repeated
active task detection, and inherited token budgets. `spawn_worker` returns
immediately, and `wait_worker` first consumes any cached terminal worker result;
otherwise it registers a worker-owned result waiter awakeable and removes that
waiter on timeout. Terminal worker results are cached on the session until
consumed so finished detached workers free active fan-out without losing the
final result.

The coordinator decomposes delegated work as a DAG of subtasks. It should spawn
all ready nodes whose dependencies are already satisfied so independent work runs
in parallel, wait only when downstream work needs a result, then use completed
worker results as context for dependent nodes or the final answer. This DAG is a
coordinator planning contract, not a worker wire schema: `spawn_worker.task`
remains the generic envelope that carries the subtask, dependency context, and
any selected skill steps the worker should follow.

Before the coordinator model synthesizes a response, the context pipeline may
append a conservative `delegation_plan` candidate for high-confidence
multi-workstream requests. Root `TurnExecution` consumes that artifact once per
admitted user message and auto-spawns dependency-free ready nodes through the
normal `spawn_worker` path, recording ordinary tool-call/tool-result events for
replay and model context. This scheduler does not authorize workers to spawn
other workers; workers stay child-local and report results back to the
coordinator. When the deterministic planner finds ready worker nodes,
`TurnExecution` raises a low requested coordinator model-loop turn cap to
`4 + 2 * ready_node_count` (bounded by active fan-out and the global session hard
cap) so the root has room to spawn, wait, and synthesize results. After
auto-spawn, the root workflow tracks the spawned worker ids, waits on worker
result awakeables before the next provider call, and appends one
`WorkerResultBundle` event when all tracked workers are terminal. History replay
renders that bundle as one coordinator-visible synthesis directive, so the model
does not need extra `list_workers` / `wait_worker` discovery turns for completed
auto-delegated workers.

### Two coordination planes

Coordinator turns can return while detached children keep running. Coordination
is split so the high-frequency path never serializes through the single-writer
parent VO. `docs/12-restate-architecture.md` is the detailed reference.

- **Telemetry plane (high-frequency, off the `Session` VO).** Turn progress is
  cadence-limited workflow state surfaced through `TurnExecution/progress` and
  `Session/progress`, not a per-tick event-log append. Heartbeats update
  `Worker` state only; `Session/progress` reads compact per-child summaries
  (`WorkerProgressSummary`) by bounded fan-in on demand through
  `Worker/progress_summary`.
- **Control plane (low-frequency, through the coordinator VO).** A narrow
  child→parent attention signal (`ChildSignalKind` =
  `Finding`/`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale`) is routed to the
  owning root `Session` via `parent_session` and recorded idempotently by
  `Session::record_child_signal`. It updates the compact `unread_child_signals`
  projection and may start one guarded resume turn when the parent is idle.

`post_message` / `WorkerMessage::FollowUp` remains the parent→child primitive.
There is no command bus and no second message queue: steering, the `needs_input`
answer, and revival all reuse the existing message path.

### Progress narration

A single per-session **narrator** keeps the user informed during the detached
window. The `Session` VO schedules a generation-guarded narration tick
(`objects/session/narration.rs`) that never calls the model inline; when its gate
opens it `.send()`s a detached `LLMGateway::narrate_session` job. That job reads
the fan-in summaries and makes **one** cheapest-chat-model call covering all
active workers plus the active coordinator step, appending one durable
`ProgressNarrated` event per period — O(1) LLM cost regardless of fan-out. With a
single active source it short-circuits with no LLM call (`model = "none"`).
Narration is default-on (`progress_narration_enabled`), gated by a coarse cadence
(`progress_narration_interval_ms`, ~20s), a content change cursor, and a
per-window cap (`progress_narration_max_per_window`), and uses the cheapest
catalog chat model unless `progress_narration_model` overrides it.

### Guarded parent resume and `needs_input`

An idle coordinator can start at most one bounded turn per resume-eligible signal,
fenced by an active-turn gate, signal-id dedupe (`pending_parent_resume_signal`),
and a per-session resume budget (`worker_resume_max_per_window` over
`worker_resume_window_ms`). Resume is conservative — only
`Blocked`/`NeedsInput`/`Failed`/`HeartbeatStale` with `ParentResumePolicy::IfIdle`,
never `Finding`/progress/plain success — and runs as the session's recorded owning
identity. The resume turn carries `RunTurnRequest.trigger = ChildSignal`, which
skips the synthetic user message; the brain renders the recorded
`WorkerParentResumeRequested` as a system directive. A running coordinator turn
also drains queued signals at context-compile time, so unread `NeedsInput`/`Blocked`
signals reach the model even without a resume.

`needs_input` is a child→parent round-trip on the same message path: the child's
`request_input` tool registers a Restate awakeable, emits a `NeedsInput` signal
carrying `input_request_id`/`input_audience`, and blocks on the awakeable against a
long timeout (`worker_input_timeout_ms`). The coordinator answers with the
`provide_worker_input` tool → `WorkerMessage::ProvideInput`, which resolves
the awakeable through `post_message`. Coordinator-audience questions are answered
autonomously. User-audience questions are exposed as `worker_input_request` SSE
frames; the next plain user reply is forwarded by the session to the worker as
`WorkerMessage::ProvideInput` instead of starting a separate root turn.

### Self-cleanup and the liveness watchdog

After a child reports terminal (cached result + result-waiter awakeable + events +
idle-wake), it schedules a generation-guarded delayed `Worker::cleanup` self-call
after `worker_cleanup_grace_ms`. Cleanup releases the child's own sandbox,
removes it from the parent's fan-out, and clears VO state — bottom-up and only once
the result is durable on the parent. A follow-up that arrives during the grace
window revives the child instead, and messages to a cleaned/terminal child are
rejected rather than re-bootstrapped. Separately, the `Session` VO arms a
generation-guarded `check_child_liveness` watchdog per active child; on a stale
heartbeat it appends `WorkerHeartbeatStale` and raises a `HeartbeatStale` signal,
exempting children parked on a `needs_input` request (`awaiting_input`).

## Workflows and Procedures

Restate workflows run internal durable jobs:

- `Consolidate`: one tenant/date memory consolidation pass.
- `KnowledgeSyncIngestion`: one tenant knowledge sync ingestion pass.
- `TurnExecution`: one durable session turn keyed by `turn_id`; runs the top-level session brain loop and calls back to `Session` on completion, cancellation, or failure.
- `WorkerTurnExecution`: one admitted worker turn keyed by `turn_id`; runs child-local LLM/tool loops and calls back to `Worker` with turn-scoped mutations.

These are workflow-shaped because rerunning the same logical job should be explicit and observable.

Skill procedures are user-authored deterministic execution plans. A skill may
declare an optional `procedure` in its `skill.moa.yaml` definition: a
`ProcedureDefinition` stores an explicit node/edge graph for branch conditions,
parallel fan-out, joins, bounded loops, connector actions, approval gates,
memory reads/writes, checkpoints, and product-visible run history.
`moa-artifacts` stores and validates the skill document shape including its
optional procedure; `moa-skills` owns the pure graph interpreter and
graph-renderable execution state; the `Skills` Restate service handles
authorization and run creation; `ProcedureExecution` owns durable execution and
node-run persistence.

Procedure nodes stay decomposable for future dashboard editing:

- deterministic nodes such as `start`, `condition`, `parallel`, `join`, and
  `end` are interpreted directly by `moa-skills`; `parallel` nodes express graph
  fan-out/join semantics, and their side effects currently execute sequentially
  in a deterministic order rather than concurrently;
- governed tool/action/skill-action nodes call existing action policy,
  review, and `ToolExecutor` services;
- `review` nodes pause the run until `Skills/decide_review` resumes or
  fails the node;
- `agent` nodes enqueue one bounded `Session` turn and wait for the existing
  `TurnExecution` result; `max_turns` caps that turn loop, not the procedure
  graph itself;
- `worker` nodes call the existing delegation path, including depth,
  fan-out, repeated-task, and budget validation;
- `memory_read` and `memory_write` nodes call the existing `Memory` service so
  tenant/contact scope, privacy, ingestion, and retrieval behavior do not fork
  inside the procedure runtime.

Adapter nodes link to inner service records such as session turns, worker
outputs, review IDs, memory hit IDs, or ingestion reports through node output.
The procedure graph remains the product-visible control plane; detailed inner
events remain in their owning service logs. Procedure improvement should operate
on skill artifact revisions and proposed patches, not by rewriting the live run
state directly.

Reusable scheduled work is anchored by the `CronJob` virtual object. Each job
key stores its cron expression, timezone, target service handler, and a version
counter that invalidates stale delayed ticks after reconfiguration.

### Background Maintenance Jobs

On boot, the orchestrator installs two periodic jobs via the `CronJob` virtual object:

- `graph_memory_compact`: fires at HH:00 UTC every hour and invokes `GraphMemoryMaint/compact`, which queues one `Consolidate` workflow for each active graph-memory tenant.
- `neon_prune_branches`: fires at 00:00, 06:00, 12:00, and 18:00 UTC and invokes `NeonMaint/prune_branches`. It is a no-op when `MOA_NEON_API_KEY` is unset.

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
- the same draft skill proposal and review paths when skill learning is compiled

Scheduling and recovery are Restate-managed in both local development and cloud
deployments.

## Segment And Learning Hooks

The orchestrator is responsible for connecting task work to learning:

- `SegmentStarted` and `SegmentCompleted` events are persisted in the event log.
- `task_segments` stores the current segment state and counters.
- Segment assessment writes `segment_assessed`.
- Experience extraction writes immutable `experience_records` from assessed segments.
- Attribution writes `experience_attributions` for skills, tools, memory, policy, and verification evidence.
- Candidate generation writes proposed `learning_candidates`; automatic promotion is gated by explicit status transitions.
- `SkillLearning` writes only draft skill artifacts and proposed skill candidates.
- `LearningReview` is the only runtime service that publishes accepted skill drafts, appends `skill_created` or `skill_improved`, and marks the candidate promoted.
- Memory consolidation writes `memory_updated`.
- Rejected skill candidates preserve draft artifacts for audit and never publish skill revisions.

This makes the learning pipeline event-sourced enough to audit and rollback without hiding updates inside model prompts.
