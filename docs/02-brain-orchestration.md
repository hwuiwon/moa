# 02 — Brain Orchestration

_Restate orchestration, hosted API runtime mode, turn execution, and sub-agents._

## Source Of Truth

`docs/12-restate-architecture.md` is the detailed Restate architecture document. This file summarizes what the current code runs:

- Cloud runtime: `moa-orchestrator`
- Client surface: HTTP routes on `moa-edge` and Restate ingress test calls
- Shared turn helpers: `crates/moa-orchestrator/src/turn/`
- Session VO: `crates/moa-orchestrator/src/objects/session/`
- Sub-agent VO: `crates/moa-orchestrator/src/objects/sub_agent/`
- Turn workflows: `crates/moa-orchestrator/src/workflows/turn_execution.rs` and `crates/moa-orchestrator/src/workflows/sub_agent_turn_execution.rs`
- CronJob VO: `crates/moa-orchestrator/src/objects/cron_job.rs`
- Pipeline assembly: `crates/moa-brain/src/pipeline/mod.rs`

## Cloud Runtime

`moa-orchestrator` is the single production binary and HTTP handler service
registered with Restate. At startup it:

1. Loads shared `MoaConfig` from flat `MOA_...` environment variables.
2. Connects to Postgres and runs session migrations.
3. Builds the Postgres session store, graph memory stack, provider registry, embedding provider, and tool router.
4. Installs an `OrchestratorCtx` singleton for handlers.
5. Binds Restate services, virtual objects, and workflows.
6. Starts the Restate endpoint and a separate health/readiness endpoint.

Default production Restate bindings:

| Restate primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO` |
| Service | `ActionReviews`, `Agents`, `ApiKeys`, `Artifacts`, `Audit`, `Authz`, `AuthzChallenges`, `Experiments`, `GraphMemoryMaint`, `Health`, `LearningReview`, `LineageAdmin`, `LLMGateway`, `Memory`, `NeonMaint`, `Privacy`, `SessionStore`, `Skills`, `Tenants`, `ToolExecutor`, `Workflows`, `WorkspaceStore`, `Whoami` |
| Workflow | `Consolidate`, `ExperimentRun`, `ExperimentTrialRun`, `TurnExecution`, `SubAgentTurnExecution` |

Feature-gated Restate bindings:

| Feature | Additional bindings |
|---|---|
| `internal-eval-runner` | `Eval` service and `EvalRun` workflow |
| `skill-learning` | `SkillLearning` workflow |

Internal application boundaries for action reviews, builtin async-authz
challenges, learning review, experiments, analytics, privacy, lineage admin,
provider routing, and memory retrieval are in-process boundaries behind the
handlers above. They are extraction seams inside the monolith, not a direction
to create internal network services.

Restate state is used for hot orchestration state: queued messages, status, child refs, active segment, cancellation flags, and child budgets. Product-visible history is written to Postgres.

`Artifacts` owns import, export, listing, validation, and publish for canonical skills, connectors, and workflows. `Workflows` exposes artifact-backed workflow run lifecycle over Restate, while `moa-workflows` owns the reusable lifecycle logic and future node interpreter. The open-ended agent loop still lives in `Session` and `TurnExecution`.

Workflow runs can carry an optional `session_id` so the product can show a procedure/workflow attached to the same support conversation. This is an association boundary, not autonomous routing: skill selection still happens inside the context pipeline, and workflow node execution remains explicit workflow runtime behavior.

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
There is no legacy session-local turn runner; `TurnExecution` owns the durable
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

The turn loop is durable because external calls and side effects are wrapped through Restate handlers or `ctx.run()` boundaries. Cancellation is delivered through a workflow promise; the workflow checks it at deterministic boundaries and races it against the in-flight LLM call. Awakeables are used for builtin async-authz challenges and sub-agent result waits, not for tool action review or turn cancellation. Skill-learning proposal generation is intentionally detached: turn completion does not wait for a draft skill proposal, and generation failures are recorded as warning events rather than turn failures.

### Lineage Sink Selection

`MOA_LINEAGE_SINK` controls how the cloud orchestrator emits lineage events:

- unset / `null` / `otel`: drops events at the sink boundary; lineage attributes are still attached to OpenTelemetry spans by the `restate_observability` helpers and are exported by the configured OTel exporter. This is the production default.
- `postgres`: writes events to the `analytics.turn_lineage` and related lineage tables in the same Postgres database the orchestrator already uses. This is recommended for local development so lineage can be queried with `psql`.

The Postgres sink runs an in-memory queue (`MpscSink`) and a background writer that drains on shutdown. Maximum queue depth and batch size come from `config.observability.lineage` in `MoaConfig`.

`TurnExecution` threads its workflow key through context compilation as the
lineage `turn_id`. Graph-memory retrieval, compiled context, generation,
citation, and online score records for the same user turn use that id so
`LineageAdmin/explain` can render one turn tree. The compiled-context record
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
  -> AdminReview: persist workspace action review, return pending-review tool result, continue
```

Workspace action reviews are decided by workspace admins through `ActionReviews`; conversation clients do not unblock turns.

## Sub-Agents

`SubAgent` is a Restate virtual object because delegated work can be conversational. It stores:

- parent session and optional parent sub-agent
- depth
- budget remaining and tokens used
- task and tool subset
- pending messages and local history
- direct-dispatch result awakeable ID
- child refs, cached terminal child results, result waiters, and cancellation reason

`SubAgent` admits conversational messages and starts at most one `SubAgentTurnExecution` workflow per active child turn. Workflow callbacks carry the admitted `turn_id`; stale responses, tool results, approval clears, and outcomes are ignored rather than mutating a newer turn.

Dispatch is bounded by depth, active fan-out, repeated active task detection, and inherited token budgets. Legacy `dispatch_sub_agent` still waits on the child's direct result awakeable. Detached `spawn_sub_agent` returns immediately, and `wait_sub_agent` first consumes any cached terminal child result; otherwise it registers a child-owned result waiter awakeable and removes that waiter on timeout. Terminal child results are cached on the parent until consumed so finished detached children free active fan-out without losing the final result.

## Workflows

MOA has two workflow-shaped execution surfaces. Restate workflows run internal durable jobs:

- `Consolidate`: one workspace/date memory consolidation pass.
- `EvalRun`: one eval replay run.
- `TurnExecution`: one durable session turn keyed by `turn_id`; runs the top-level session brain loop and calls back to `Session` on completion, cancellation, or failure.
- `SubAgentTurnExecution`: one admitted sub-agent turn keyed by `turn_id`; runs child-local LLM/tool loops and calls back to `SubAgent` with turn-scoped mutations.

These are workflow-shaped because rerunning the same logical job should be explicit and observable.

Artifact-backed workflows are user-authored `WorkflowDefinition` documents for explicit node graphs, branch conditions, connector actions, approval gates, checkpoints, and product-visible run history. `moa-artifacts` stores and validates the workflow document shape; `moa-workflows` creates and mutates durable workflow runs; the `Workflows` Restate service handles authorization and service binding. Workflow improvement should operate on artifact revisions and proposed patches, not by rewriting the live run state directly.

Reusable scheduled work is anchored by the `CronJob` virtual object. Each job
key stores its cron expression, timezone, target service handler, and a version
counter that invalidates stale delayed ticks after reconfiguration.

### Background Maintenance Jobs

On boot, the orchestrator installs two periodic jobs via the `CronJob` virtual object:

- `graph_memory_compact`: fires at HH:00 UTC every hour and invokes `GraphMemoryMaint/compact`, which queues one `Consolidate` workflow for each active graph-memory workspace.
- `neon_prune_branches`: fires at 00:00, 06:00, 12:00, and 18:00 UTC and invokes `NeonMaint/prune_branches`. It is a no-op when `MOA_NEON_API_KEY` is unset.

To inspect the schedule:

```bash
curl http://localhost:10010/CronJob/graph_memory_compact/status
```

To pause or resume a job without clearing its config:

```bash
curl -X POST http://localhost:10010/CronJob/graph_memory_compact/pause
curl -X POST http://localhost:10010/CronJob/graph_memory_compact/resume
```

To install a custom schedule, post a new body to `/CronJob/{key}/configure`
and bump the bootstrap idempotency-key version suffix in code.

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
- `LearningReview` is the only runtime service that publishes accepted skill drafts, materializes active `moa.skill` rows, appends `skill_created` or `skill_improved`, and marks the candidate promoted.
- Memory consolidation writes `memory_updated`.
- Rejected skill candidates preserve draft artifacts for audit and never update active skills.

This makes the learning pipeline event-sourced enough to audit and rollback without hiding updates inside model prompts.
