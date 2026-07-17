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
- Execution workflows: `crates/moa-orchestrator/src/workflows/execution_run.rs` and `crates/moa-orchestrator/src/workflows/execution_task.rs`
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
| Service | `ActionReviews`, `AgentDefinitions`, `Agents`, `AdminMaintenance`, `ApiKeys`, `Artifacts`, `Authz`, `AuthzChallenges`, `Contacts`, `Eval`, `Execution`, `Experiments`, `GraphMemoryMaint`, `Knowledge`, `LearningReview`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `ActionPolicy` |
| Workflow | `ExecutionRun`, `ExecutionTask`, `KnowledgeSyncIngestion`, `Consolidate`, `TurnExecution`, `WorkerTurnExecution`, `ExperimentRun`, `ExperimentTrialRun` |

Feature-gated Restate bindings:

| Feature | Additional bindings |
|---|---|

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
skills, connectors, actions, and agents. `moa-execution` owns execution-plan
compilation, pure scheduling, budgets, completion, and run/task persistence.
The `Execution` service exposes start, status, list, cancel, review, signal, and
bounded task-result operations. `ExecutionRun` and `ExecutionTask` own durable
graph execution. The open-ended agent loop remains in `Session` and
`TurnExecution`.

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

`TurnExecution` first selects one mode, then owns its turn mechanics. Ordinary
user language is classified by at most one strict, no-tools auxiliary-model
call before mode execution. Therefore `respond` means one user-facing response
call after at most one classifier call, not one total provider call. Trusted
template invocation, typed Act escalation, blank-objective preflight, and
internal synthesis bypass the classifier. Any uncertain or malformed
classifier result selects `act` without retry or planner fallback.

- `needs_input` appends one deterministic clarification carrying the bounded
  missing fields returned by routing.

- `respond` makes one model call with no tools and no planning call.
- `act` runs the existing bounded root model/tool loop and may use
  conversational workers.
- `run` instantiates a pinned skill template or compiles a strict generated
  plan, persists it, starts `ExecutionRun` detached, and returns acceptance
  without polling it from the root model.

An `act` turn may escalate to `run` after discovering durable, high-fan-out,
resumable, or review-bearing work. Task difficulty by itself is not a routing
signal.

1. Build a `CompletionRequest` from session events and the context pipeline.
2. Ensure a task segment exists or roll to a new segment when query rewrite marks `is_new_task`.
3. Select `respond`, `act`, `run`, or `needs_input` through trusted control facts
   or one bounded auxiliary classifier call; persist the redacted normalized
   route audit before mode execution.
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
  tables. In cloud, the journal path must be an explicit persistent mounted
  path, not pod-local temp storage.
- `clickhouse`: the same durable sink, but fails startup when the
  `[clickhouse]` config section is missing. Useful to make the backend choice
  explicit in a deployment.

The durable sink runs an in-memory queue (`MpscSink`) and a background writer
that drains and replays the durable journal. Queue pressure can drop only
explicitly configured lossy telemetry; audit-class events are not accepted
before the journal append succeeds. Maximum queue depth and batch size come
from `config.observability.lineage` in `MoaConfig`.

#### ClickHouse Row Backend

The optional top-level `[clickhouse]` config section (or `MOA_CLICKHOUSE_URL`
plus `MOA_CLICKHOUSE_USER`/`MOA_CLICKHOUSE_PASSWORD`; empty values mean unset)
selects where the durable sink lands `turn_lineage` rows:

- **Absent (default):** everything writes to Postgres/Timescale exactly as
  before.
- **Present:** `turn_lineage` rows are inserted into ClickHouse
  (`<database>.turn_lineage`, a `ReplacingMergeTree` ordered by
  `(storage_partition_id, ts, turn_id, record_kind)` with a
  `lineage_ttl_days` TTL, bootstrapped at startup). Lineage explain reads,
  typed lineage queries, knowledge retrieval traces, and tenant-offboarding
  deletes follow the same switch.

Postgres always stays attached under both backends: `analytics.scores` (its
rollups join OLTP experiment tables), lineage dead letters, and the compliance
chain state do not move. Compliance hash chaining and `lineage verify` require
the Postgres backend; when a compliance-enabled partition's rows land in
ClickHouse the writer emits `moa_lineage_compliance_chain_skipped_total` and a
warning, and rows carry only their per-row canonical `integrity_hash`.

Locally, `docker compose --profile clickhouse up -d` starts a ClickHouse
server on host port 10061; set `MOA_CLICKHOUSE_URL=http://clickhouse:8123`,
`MOA_CLICKHOUSE_USER=moa`, and `MOA_CLICKHOUSE_PASSWORD=dev` in `.env` to
route the compose orchestrator and edge at it.

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

Delegation is owned by the root coordinator in `act`. Workers are bounded
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

Workers support interactive, steerable delegation inside `act`. They are not
plan nodes, map items, reducers, or the bulk DAG substrate. Work that needs an
explicit dependency graph, durable joins, scalable map materialization, review
waits, or exact coverage uses `ExecutionRun` and stable `ExecutionTask` rows.
Conversational worker fan-out limits therefore do not impose an execution-map
fan-out cap.

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

## Workflows And Execution Runs

Restate workflows run internal durable jobs:

- `Consolidate`: one tenant/date memory consolidation pass.
- `KnowledgeSyncIngestion`: one tenant knowledge sync ingestion pass.
- `TurnExecution`: one durable session turn keyed by `turn_id`; runs the top-level session brain loop and calls back to `Session` on completion, cancellation, or failure.
- `WorkerTurnExecution`: one admitted worker turn keyed by `turn_id`; runs child-local LLM/tool loops and calls back to `Worker` with turn-scoped mutations.
- `ExecutionRun`: one immutable goal contract and active plan keyed by `run_uid`.
- `ExecutionTask`: one stable logical node or map item keyed by its task identity.

These are workflow-shaped because rerunning the same logical job should be
explicit and observable.

`ExecutionRun` loads the persisted canonical plan and asks the pure interpreter
for every ready logical task. It materializes stable rows keyed by
`(run_uid, node_id, item_key)` and submits all ready tasks durably. There is no
application active-worker count or execution fan-out constant. Run budgets
bound logical task count; Restate concurrency rules and provider pacing queue
physical work.

`ExecutionTask` atomically reserves its worst-case integer cost, token, task,
tool-call, retrieved-byte, and deadline allowance before dispatch. A failed
reservation starts no work. It resolves only compiler-approved references,
executes one governed capability or bounded agent task, reconciles actual usage,
persists citations and output, and completes through a generation fence so a
stale attempt cannot overwrite newer work.

The plan is an acyclic graph with exactly `Capability`, `Agent`, `Map`,
`Reduce`, `Review`, `WaitSignal`, and `Output`. A map task is only a capability
or agent and cannot nest another map. Agent tasks can use declared
instruction-only skills and capabilities with bounded turns and budgets. They
cannot mutate the graph. Unexpected conditions return typed `NeedsInput` or
`NeedsReplan`; every amendment is compiled, authorization-narrowing, budgeted,
persisted in `plan_history`, and applied only to pending or downstream work.
Repeated hashes, recurring failure fingerprints, no progress, deadline, or
resource exhaustion terminate with exact partial/blocked coverage instead of an
infinite loop.

Cancellation prevents new reservations and leaves completed task results
queryable. A run cannot become `completed` until every immutable goal-contract
requirement and completion check passes. Terminal state emits compact aggregate
output, citations, failures, and gaps to the owning session. The session starts
at most one deduplicated synthesis turn for the originating user sequence; it
does not ingest every raw map output or poll the run through the root model.

The deterministic and sampled validation of these claims is defined in
[Execution Honesty Evaluation](eval/execution-honesty.md). Those checks consume
the same persisted projection, task rows, planning audits, and bounded session
event evidence as runtime inspection; they do not reconstruct success from a
prose transcript.

Reusable scheduled work is anchored by the `CronJob` virtual object. Each job
key stores its cron expression, timezone, target service handler, and a version
counter that invalidates stale delayed ticks after reconfiguration.

### Background Maintenance Jobs

On boot, the orchestrator installs four periodic jobs via the `CronJob`
virtual object:

- `graph_memory_compact`: fires at HH:00 UTC every hour and invokes
  `GraphMemoryMaint/compact`, which queues one `Consolidate` workflow for each
  active graph-memory tenant.
- `vector_sync_outbox_drain`: fires every minute and invokes
  `GraphMemoryMaint/sync_vectors` with `limit = 512`.
- `segment_materialized_views_refresh`: fires every 15 minutes and invokes
  `SessionStore/refresh_segment_materialized_views`.
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
