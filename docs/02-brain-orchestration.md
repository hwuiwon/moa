# 02 — Brain Orchestration

_Restate orchestration, local runtime mode, turn execution, and sub-agents._

## Source Of Truth

`docs/12-restate-architecture.md` is the detailed Restate architecture document. This file summarizes what the current code runs:

- Cloud runtime: `moa-orchestrator`
- Local runtime: `moa-orchestrator-local`
- Shared turn helpers: `crates/moa-orchestrator/src/turn/`
- Session VO: `crates/moa-orchestrator/src/objects/session.rs`
- Sub-agent VO: `crates/moa-orchestrator/src/objects/sub_agent.rs`
- Pipeline assembly: `crates/moa-brain/src/pipeline/mod.rs`

## Cloud Runtime

`moa-orchestrator` is an HTTP handler service registered with Restate. At startup it:

1. Loads `OrchestratorConfig` from environment.
2. Connects to Postgres and runs session migrations.
3. Builds the Postgres session store, memory store, provider registry, embedding provider, and tool router.
4. Installs an `OrchestratorCtx` singleton for handlers.
5. Binds Restate services, virtual objects, and workflows.
6. Starts the Restate endpoint and a separate health/readiness endpoint.

Bound surfaces:

| Restate primitive | Handlers |
|---|---|
| Virtual Object | `Session`, `SubAgent`, `Workspace` |
| Service | `Health`, `SessionStore`, `IntentManager`, `LLMGateway`, `MemoryStore`, `ToolExecutor`, `WorkspaceStore` |
| Workflow | `Consolidate`, `IntentDiscovery` |

Restate state is used for hot orchestration state: queued messages, status, pending approvals, child refs, active segment, cancellation flags, and child budgets. Product-visible history is written to Postgres.

## Session Flow

```text
client sends message
  -> SessionStore creates/loads session metadata
  -> Session::set_meta initializes VO state when needed
  -> Session::post_message appends the message and sets status running
  -> Session::run_turn executes one turn
  -> post_message loops until idle, blocked, cancelled, or max turns reached
```

`Session::post_message` is serialized by Restate's single-writer-per-key semantics. Concurrent messages for the same session queue behind the active invocation instead of requiring application locks.

`Session::run_turn` delegates most turn mechanics to `TurnRunner` through `SessionTurnAdapter`:

1. Build a `CompletionRequest` from session events and the context pipeline.
2. Ensure a task segment exists or roll to a new segment when query rewrite marks `is_new_task`.
3. Classify the segment against active tenant intents when embeddings and active intents are available.
4. Call `LLMGateway`.
5. Persist assistant output and tool calls.
6. Route tool execution through `ToolExecutor`.
7. Record tool usage, skill activation, token usage, and turn counts on the active segment.
8. Apply turn outcome and update session status.
9. Score idle, cancelled, or completed segments and append `learning_log` entries.

The turn loop is durable because external calls and side effects are wrapped through Restate handlers or `ctx.run()` boundaries.

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
- `IntentDiscovery`: one tenant intent-discovery pass over recent undefined task segments.

These are workflow-shaped because rerunning the same logical job should be explicit and observable.

## Local Runtime

`moa-orchestrator-local` implements `BrainOrchestrator` with Tokio tasks and broadcast channels. It is used by `moa-cli`, `moa-runtime`, and `moa-desktop`.

Local mode still uses:

- `PostgresSessionStore`
- `FileMemoryStore`
- the same context pipeline
- the same tool router and permission store
- the same skill distillation and learning-log paths when a learning store is present

The main difference is scheduling and recovery. Local tasks are process-local; cloud sessions are Restate-managed.

## Segment And Learning Hooks

The orchestrator is responsible for connecting task work to learning:

- `SegmentStarted` and `SegmentCompleted` events are persisted in the event log.
- `task_segments` stores the current segment state and counters.
- Intent classification writes `intent_classified`.
- Resolution scoring writes `resolution_scored`.
- Memory consolidation writes `memory_updated`.
- Skill distillation and improvement write `skill_created` and `skill_improved`.
- Intent discovery and admin actions write intent learning events.

This makes the learning pipeline event-sourced enough to audit and rollback without hiding updates inside model prompts.
