# 02 — Brain Orchestration

_Restate orchestration, hosted API runtime mode, turn execution, and sub-agents._

## Source Of Truth

`docs/12-restate-architecture.md` is the detailed Restate architecture document. This file summarizes what the current code runs:

- Cloud runtime: `moa-orchestrator`
- Client surface: HTTP routes on `moa-edge` and Restate ingress test calls
- Shared turn helpers: `crates/moa-orchestrator/src/turn/`
- Session VO: `crates/moa-orchestrator/src/objects/session.rs`
- Sub-agent VO: `crates/moa-orchestrator/src/objects/sub_agent.rs`
- CronJob VO: `crates/moa-orchestrator/src/objects/cron_job.rs`
- Pipeline assembly: `crates/moa-brain/src/pipeline/mod.rs`

## Cloud Runtime

`moa-orchestrator` is an HTTP handler service registered with Restate. At startup it:

1. Loads `OrchestratorConfig` from environment.
2. Connects to Postgres and runs session migrations.
3. Builds the Postgres session store, graph memory stack, provider registry, embedding provider, and tool router.
4. Installs an `OrchestratorCtx` singleton for handlers.
5. Binds Restate services, virtual objects, and workflows.
6. Starts the Restate endpoint and a separate health/readiness endpoint.

Bound surfaces:

| Restate primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `SubAgent`, `Workspace`, `CronJob`, `IngestionVO` |
| Service | `AgentRegistry`, `AgentTemplates`, `Agents`, `Approvals`, `ApiKeys`, `Audit`, `Authz`, `GraphMemoryMaint`, `Health`, `LLMGateway`, `NeonMaint`, `SessionStore`, `Tenants`, `ToolExecutor`, `WorkspaceStore`, `Whoami` |
| Workflow | `Consolidate`, `TurnExecution` |

Restate state is used for hot orchestration state: queued messages, status, pending approvals, child refs, active segment, cancellation flags, and child budgets. Product-visible history is written to Postgres.

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
`Session::run_turn` is retained only as a legacy wire-compatible handler and no
longer drives turn execution.

`TurnExecution` owns the turn mechanics:

1. Build a `CompletionRequest` from session events and the context pipeline.
2. Ensure a task segment exists or roll to a new segment when query rewrite marks `is_new_task`.
3. Call `LLMGateway`.
4. Persist assistant output and tool calls.
5. Route tool execution through `ToolExecutor`.
6. Record tool usage, skill activation, token usage, and turn counts on the active segment.
7. Apply turn outcome and update session status.
8. Score idle, cancelled, or completed segments and append `learning_log` entries.

The turn loop is durable because external calls and side effects are wrapped through Restate handlers or `ctx.run()` boundaries. Cancellation is delivered through a workflow promise plus awakeable ID; the workflow checks it at deterministic boundaries and races it against the in-flight LLM call.

### Lineage Sink Selection

`MOA_LINEAGE_SINK` controls how the cloud orchestrator emits lineage events:

- unset / `null` / `otel`: drops events at the sink boundary; lineage attributes are still attached to OpenTelemetry spans by the `restate_observability` helpers and are exported by the configured OTel exporter. This is the production default.
- `postgres`: writes events to the `analytics.turn_lineage` and related lineage tables in the same Postgres database the orchestrator already uses. This is recommended for local development so lineage can be queried with `psql`.

The Postgres sink runs an in-memory queue (`MpscSink`) and a background writer that drains on shutdown. Maximum queue depth and batch size come from `config.observability.lineage` in `MoaConfig`.

### Provider Overrides For Test Runs

`MOA_PROVIDERS_OVERRIDE` is a dev/CI-only startup switch for replacing normal
LLM providers inside `moa-orchestrator`:

- unset: use providers configured from normal API keys.
- `scripted:<path>`: use a JSON fixture with deterministic responses.
- `mock:<seed>`: use the built-in deterministic mock response.

The orchestrator refuses to start with an override when the environment is
`prod` or `production`. The checked-in load-test fixture lives at
`crates/moa-loadtest/scripts/perf-gate.json`; see
`docs/testing/providers-override.md` for the script format.

## Approvals

Risky tool calls emit `ApprovalRequested` events. In cloud mode the blocked invocation stores an awakeable ID in VO state and event payload. The gateway or REST surface resolves the approval by calling the appropriate handler with an `ApprovalDecision`.

```text
Tool call needs approval
  -> create awakeable
  -> persist ApprovalRequested with awakeable id
  -> UI renders approval
  -> user decides
  -> approval handler resolves the blocked turn
```

Sub-agent approvals include `sub_agent_id` and route back through the parent user's approval surface.

## Sub-Agents

`SubAgent` is a Restate virtual object because delegated work can be conversational. It stores:

- parent session and optional parent sub-agent
- depth
- budget remaining and tokens used
- task and tool subset
- pending messages and local history
- result awakeable ID
- child refs and cancellation reason

Dispatch is bounded by depth, fan-out, repeated task detection, and inherited token budgets. Parent sessions receive results through awakeables or status queries.

## Workflows

Only one-shot background jobs use workflows:

- `Consolidate`: one workspace/date memory consolidation pass.
- `TurnExecution`: one durable session turn keyed by `turn_id`; runs the top-level session brain loop and calls back to `Session` on completion, cancellation, or failure.

These are workflow-shaped because rerunning the same logical job should be explicit and observable.

Reusable scheduled work is anchored by the `CronJob` virtual object. Each job
key stores its cron expression, timezone, target service handler, and a version
counter that invalidates stale delayed ticks after reconfiguration.

### Background Maintenance Jobs

On boot, the orchestrator installs two periodic jobs via the `CronJob` virtual object:

- `graph_memory_compact`: fires at HH:00 UTC every hour and invokes `GraphMemoryMaint/compact`. It is currently a no-op shell.
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
- the same skill distillation and learning-log paths when a learning store is present

Scheduling and recovery are Restate-managed in both local development and cloud
deployments.

## Segment And Learning Hooks

The orchestrator is responsible for connecting task work to learning:

- `SegmentStarted` and `SegmentCompleted` events are persisted in the event log.
- `task_segments` stores the current segment state and counters.
- Resolution scoring writes `resolution_scored`.
- Memory consolidation writes `memory_updated`.
- Skill distillation and improvement write `skill_created` and `skill_improved`.

This makes the learning pipeline event-sourced enough to audit and rollback without hiding updates inside model prompts.
