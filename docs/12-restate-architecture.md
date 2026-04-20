# 12 — Restate Architecture

_Durable execution on Restate Virtual Objects and Workflows._

---

## Purpose

This document specifies how MOA uses Restate as its sole durable execution engine. It defines the mapping of MOA's session/brain/hand model onto Restate's primitives (Services, Virtual Objects, Workflows), handler signatures in Rust, state and journal strategy, and Kubernetes deployment.

Scope: all session orchestration, sub-agent dispatch, tool execution, approval flows, memory consolidation, and scheduled work. Out of scope: data plane primitives (Postgres event log, Daytona sandboxes, LLM gateway) — those remain as specified in `05`, `06`, and the v2 architecture doc.

---

## Why Restate (one-paragraph rationale)

Restate's Rust SDK is GA at v0.8 with idiomatic `#[restate_sdk::service]`, `#[restate_sdk::object]`, and `#[restate_sdk::workflow]` macros. Restate's **Virtual Objects provide single-writer-per-key semantics** that map exactly onto "one agent session = one actor," eliminating application-level locking and queue coordination. The Restate server is a single Rust binary with embedded RocksDB — no external broker, no Cassandra, no multi-service control plane.

---

## Core Restate concepts

Three handler types. Choose the right one per component, not the one that seems most powerful.

### Service

Stateless RPC handler. No keyed state, no serialization of concurrent invocations. Invocations are durable: each call is journaled, retries are automatic, side effects inside `ctx.run()` are recorded once.

```rust
#[restate_sdk::service]
trait ToolExecutor {
    async fn execute(ctx: Context<'_>, req: ToolCallRequest) -> Result<ToolOutput, HandlerError>;
}
```

**Use for**: tool execution, LLM gateway calls, memory reads/writes, embedding generation, anything that can be called from many places and doesn't hold state between calls.

### Virtual Object

Keyed actor with durable state. **Single-writer semantics per key**: concurrent invocations on the same key queue up and execute one at a time. State is a key-value store scoped to the object's key, survives pod restarts, persists beyond any single invocation.

```rust
#[restate_sdk::object]
trait Session {
    async fn post_message(ctx: ObjectContext<'_>, msg: UserMessage) -> Result<(), HandlerError>;

    #[shared]
    async fn status(ctx: SharedObjectContext<'_>) -> Result<SessionStatus, HandlerError>;
}
```

`#[shared]` handlers are read-only and run concurrently — they don't participate in the single-writer queue. Use them for status queries, never for mutations.

**Use for**: sessions, sub-agents, workspace coordination, user-scoped state — anything with identity and a serialized event stream.

### Workflow

A specialized Virtual Object that runs exactly once per ID. Has one `run` method (the workflow body) plus any number of signal/query handlers. After completion, the workflow ID is dead — you cannot reuse it.

```rust
#[restate_sdk::workflow]
trait Consolidate {
    async fn run(ctx: WorkflowContext<'_>, req: ConsolidateRequest) -> Result<ConsolidateReport, HandlerError>;
}
```

**Use for**: one-shot tasks with a well-defined start and end where re-invocation must be structurally impossible. Memory consolidation runs. Source ingestion. Never use for agents — agents are conversational by nature.

### Primitives inside handlers

| Primitive | Purpose |
|---|---|
| `ctx.run("name", \|\| async { ... })` | Durable side effect — recorded once, replayed from journal on retry |
| `ctx.sleep(duration)` | Durable sleep — survives pod restart |
| `ctx.get::<T>("key")` / `ctx.set("key", v)` | Object/workflow state KV |
| `ctx.awakeable::<T>()` | External signal — returns `(id, future)`; pause until resolved via API |
| `ctx.service_client::<S>().handler(req)` | Call a Service |
| `ctx.object_client::<O>(key).handler(req)` | Call a Virtual Object |
| `ctx.workflow_client::<W>(id).run(req)` | Start a Workflow |
| `ctx.rand()`, `ctx.time()` | Deterministic sources for replay safety |

**The rule that matters**: anything non-deterministic or side-effectful must go through `ctx.run()` or a typed primitive. Direct `reqwest::get()`, `std::time::SystemTime::now()`, or `rand::random()` inside a handler is a replay bug waiting to happen.

---

## Mapping MOA to Restate primitives

The mapping is deliberate and reversible only with significant cost. Each choice below has a specific reason.

| MOA concept | Restate primitive | Key / ID |
|---|---|---|
| Session | **Virtual Object** (`Session`) | `session_id` |
| User turn | One invocation on `Session::post_message` | serialized by VO |
| Sub-agent | **Virtual Object** (`SubAgent`) | `sub_agent_id` |
| Sub-agent turn | Invocation on `SubAgent::post_message` | serialized by VO |
| Tool call | **Service** handler (`ToolExecutor::execute`) | — |
| LLM call | Service handler (`LLMGateway::complete`) called from Session | — |
| Hand execution | Service handler (`HandRunner::execute`) | — |
| Memory read/write | Service handler (`MemoryStore::*`) | — |
| Session event log append | Service handler (`SessionStore::append_event`) | — |
| Workspace | Virtual Object (`Workspace`) | `workspace_id` |
| Consolidation / dream cycle | **Workflow** (`Consolidate`) — scheduled from `Workspace` VO | `workspace_id:YYYY-MM-DD` |
| Source ingestion | **Workflow** (`IngestSource`) | `source_hash` |
| Approval request | `ctx.awakeable::<Decision>()` inside turn, resolved by gateway | — |
| Session cancellation | Handler on `Session` VO, sets a cancel flag in state | — |

### Why session is a Virtual Object, not a Workflow

Sessions are long-lived and receive many user messages over time. Workflows run once and die; a session that runs for hours and accepts dozens of messages fits the VO model, not the workflow model. The VO's **single-writer queue is exactly the serialization we want**: if a user sends three messages while a turn is running, they queue up and process in order without any application-level locking. This is the pattern LangGraph Platform and Letta both converged on.

### Why sub-agents are Virtual Objects

Sub-agents in MOA are conversational by default. A parent session may dispatch a specialist sub-agent, receive intermediate results, ask follow-up questions, request refinements, or escalate back to the parent LLM — all across multiple turns. This is the same interaction model as the user↔session relationship, one level deeper. The VO primitive fits naturally: `SubAgent` shares nearly all of `Session`'s state shape and turn loop, differing only in who sends messages (parent VO instead of gateway) and how results are returned (awakeables or direct calls instead of platform rendering).

One-shot tasks that happen to be LLM-driven — research-and-summarize, classify-this-document, extract-entities — are **not sub-agents**. They are tool calls that happen to route through an LLM, implemented as service handlers or dedicated Workflows (e.g., `Summarize`), not as `SubAgent` instances.

### Why tools are Services, not Workflows

Tool calls are ephemeral and called from within Session VO turns via typed clients. Service semantics (no keyed state, durable per-invocation) are correct. Wrapping them in Workflows adds no value and pollutes the journal with extra workflow starts.

### Why only Consolidate and IngestSource are Workflows

These are genuine one-shot operations where re-invocation must be structurally impossible:
- **Consolidate**: running it twice on the same workspace for the same date would double-process pages and corrupt the wiki. Workflow's runs-once-per-ID guarantee is the correctness property we want.
- **IngestSource**: ingesting the same source document twice produces duplicate pages with different summaries. Keying the workflow on `source_hash` makes re-ingestion a no-op.

Everything else is either a conversational actor (VO) or a stateless operation (Service).

### What stays in Postgres, not in Restate state

Restate state is **fast KV for orchestration**, not a database. Put in VO state only what the orchestration needs on the hot path: current turn status, awakeable IDs awaiting resolution, active tool call handles, the last N turns for context assembly. Everything else — full event log, billing records, memory pages, embeddings, audit trail — goes to Postgres via `SessionStore` service calls. **Product-visible data of record is Postgres. Restate state is the working memory of a live session.**

---

## Handler signatures (concrete Rust)

Code below is illustrative, not final. Full types live in `moa-core`; the `moa-orchestrator` crate owns these handlers.

### `Session` Virtual Object

```rust
use restate_sdk::prelude::*;

#[restate_sdk::object]
trait Session {
    /// Append a user message and run turns until idle or an awakeable blocks.
    async fn post_message(ctx: ObjectContext<'_>, msg: UserMessage) -> Result<(), HandlerError>;

    /// Resolve an outstanding approval awakeable.
    async fn approve(ctx: ObjectContext<'_>, decision: ApprovalDecision) -> Result<(), HandlerError>;

    /// Soft cancel (finish current tool, then stop) or hard cancel (abort).
    async fn cancel(ctx: ObjectContext<'_>, mode: CancelMode) -> Result<(), HandlerError>;

    /// Read-only status query, runs concurrently with mutations.
    #[shared]
    async fn status(ctx: SharedObjectContext<'_>) -> Result<SessionStatus, HandlerError>;

    /// Called by self at turn boundaries. Not exposed externally.
    async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError>;

    /// Called 72h after completion to clear VO state.
    async fn destroy(ctx: ObjectContext<'_>) -> Result<(), HandlerError>;
}
```

### `SubAgent` Virtual Object

Nearly identical to `Session`, differing only in input/output channels.

```rust
#[restate_sdk::object]
trait SubAgent {
    /// Parent dispatches a message (initial task or follow-up).
    async fn post_message(ctx: ObjectContext<'_>, msg: SubAgentMessage) -> Result<(), HandlerError>;

    /// Parent requests a result or intermediate summary.
    #[shared]
    async fn status(ctx: SharedObjectContext<'_>) -> Result<SubAgentStatus, HandlerError>;

    /// Parent cancels the sub-agent.
    async fn cancel(ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError>;

    /// Approval handler (sub-agents can request approvals; routed through parent to user).
    async fn approve(ctx: ObjectContext<'_>, decision: ApprovalDecision) -> Result<(), HandlerError>;

    /// Internal turn loop.
    async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError>;

    /// Destroy after parent marks done.
    async fn destroy(ctx: ObjectContext<'_>) -> Result<(), HandlerError>;
}
```

### `Consolidate` Workflow

```rust
#[restate_sdk::workflow]
trait Consolidate {
    async fn run(ctx: WorkflowContext<'_>, req: ConsolidateRequest)
        -> Result<ConsolidateReport, HandlerError>;
}
```

### `ToolExecutor` Service

```rust
#[restate_sdk::service]
trait ToolExecutor {
    async fn execute(ctx: Context<'_>, req: ToolCallRequest)
        -> Result<ToolOutput, HandlerError>;
}
```

### `LLMGateway` Service

```rust
#[restate_sdk::service]
trait LLMGateway {
    async fn complete(ctx: Context<'_>, req: CompletionRequest)
        -> Result<CompletionResponse, HandlerError>;

    async fn stream_complete(ctx: Context<'_>, req: CompletionRequest)
        -> Result<CompletionStreamHandle, HandlerError>;
}
```

The streaming variant returns a handle that references a stream held in the gateway pod; the Session VO polls the handle via subsequent service calls. Restate does not support true streaming return values from handlers.

### Session turn loop (shape)

```rust
impl Session for SessionImpl {
    async fn post_message(ctx: ObjectContext<'_>, msg: UserMessage) -> Result<(), HandlerError> {
        // Append to VO state and to Postgres event log.
        let mut pending = ctx.get::<Vec<UserMessage>>("pending").await?.unwrap_or_default();
        pending.push(msg.clone());
        ctx.set("pending", pending);

        ctx.service_client::<SessionStoreClient>()
            .append_event(SessionEvent::UserMessage(msg))
            .call()
            .await?;

        // Drive turns until no more pending messages or an awakeable pauses us.
        // Self-call via object_client so each turn is its own invocation —
        // explicit journal entry per turn, visible in the admin UI for debugging.
        loop {
            let outcome = ctx.object_client::<SessionClient>(ctx.key())
                .run_turn()
                .call()
                .await?;

            match outcome {
                TurnOutcome::Continue => continue,
                TurnOutcome::Idle | TurnOutcome::WaitingApproval | TurnOutcome::Cancelled => break,
            }
        }
        Ok(())
    }

    async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
        // 1. Build context from VO state + Postgres (via Service).
        let ctx_bundle = build_working_context(&ctx).await?;

        // 2. Call LLM via gateway Service. Gateway wraps the external call in ctx.run internally.
        let response = ctx.service_client::<LLMGatewayClient>()
            .complete(ctx_bundle.into())
            .call()
            .await?;

        // 3. Handle tool calls in the response.
        for tool_call in response.tool_calls {
            if requires_approval(&tool_call, &ctx).await? {
                let (awakeable_id, awakeable) = ctx.awakeable::<ApprovalDecision>();
                ctx.set("pending_approval", &awakeable_id);

                // Persist the request so the gateway can fetch it and render buttons.
                ctx.service_client::<SessionStoreClient>()
                    .append_event(SessionEvent::ApprovalRequested {
                        tool_call: tool_call.clone(),
                        awakeable_id: awakeable_id.clone(),
                    })
                    .call()
                    .await?;

                let decision = awakeable.await?;  // durable wait
                ctx.clear("pending_approval");

                if matches!(decision, ApprovalDecision::Deny { .. }) {
                    continue; // feed denial back to LLM next turn
                }
            }

            // Execute via ToolExecutor service.
            let result = ctx.service_client::<ToolExecutorClient>()
                .execute(tool_call)
                .call()
                .await?;

            ctx.service_client::<SessionStoreClient>()
                .append_event(SessionEvent::ToolResult(result))
                .call()
                .await?;
        }

        Ok(if response.stop_reason.is_final() { TurnOutcome::Idle } else { TurnOutcome::Continue })
    }
}
```

This is the heart of the orchestrator. A few things worth noticing:

- **Durable approval wait**: `ctx.awakeable()` returns a future that is transparently durable — the VO can be evicted from memory, the pod can restart, and when approval arrives the handler resumes at the `.await` point. No polling loop, no timers, no bespoke signal adapters.
- **No explicit retries**: tool failures propagate up; Restate retries the whole invocation per the handler's retry policy. Idempotency wrappers inside `ToolExecutor` prevent double-execution.
- **Journal growth**: every `ctx.service_client(...).call()` and every `ctx.awakeable()` adds a journal entry. A 20-tool turn generates ~60 entries. For hours-long sessions, this is the reason per-handler retention matters.
- **Debuggability via self-call**: the loop uses `object_client().run_turn().call()` rather than inlining the turn body. Each turn becomes its own invocation visible in `restate invocations list`, at the cost of one extra journal entry per turn. Worth it.

---

## Sub-agent dispatch

Parent VO creates a SubAgent VO and sends messages. The sub-agent runs its own turn loop independently; the parent either awaits completion via a pre-registered awakeable or polls status.

```rust
// Parent: dispatch a sub-agent
let sub_id = format!("{}-{}", ctx.key(), ctx.rand_uuid());

// Record the dispatch so parent can query status later.
let mut children = ctx.get::<Vec<String>>("children").await?.unwrap_or_default();
children.push(sub_id.clone());
ctx.set("children", children);

// Pre-register awakeable for result delivery.
let (result_awakeable_id, result_future) = ctx.awakeable::<SubAgentResult>();

// Send initial task message. Sub-agent will start its turn loop.
ctx.object_client::<SubAgentClient>(sub_id.clone())
    .post_message(SubAgentMessage::InitialTask {
        task,
        tools,
        budget,
        parent_session: ctx.key().into(),
        depth: current_depth + 1,
        result_awakeable_id,
    })
    .send(); // fire and forget; parent doesn't block on the dispatch itself

// Durable wait for sub-agent to resolve the awakeable with its result.
let result = result_future.await?;
```

**Fork-bomb prevention is enforced in the sub-agent's turn body**: each `SubAgent::run_turn` reads depth from VO state (set at dispatch), rejects dispatching children if depth >= 3, caps fan-out to 4 children per node, and refuses spawning identical task hashes already in the parent trace (loop-detection). Global per-session concurrent sub-agent count lives in the root `Session` VO state and propagates down via message headers. Token budget inheritance is enforced the same way — the child's budget is deducted from the parent's remaining budget at dispatch; violations return `HandlerError::BudgetExceeded` from `post_message`.

---

## Approvals via awakeables

Approvals are the canonical Restate pattern for "pause until human decides." The flow:

```
Session VO turn
  → ctx.awakeable::<ApprovalDecision>() returns (id, future)
  → id stored in VO state + written to Postgres event log
  → gateway renders approval card, sends id to user UI
  → user clicks button
  → gateway calls Restate admin API: resolve_awakeable(id, decision)
  → VO handler resumes at future.await
```

The gateway never holds the turn's execution state. Restate does. This removes an entire class of "gateway crashed, lost pending approvals" bugs that plague simpler architectures.

**Timeout**: awakeables can race with `ctx.sleep(30.minutes())` using a select pattern. Timed-out approvals auto-deny with a specific reason fed back to the LLM so it can re-plan.

**Sub-agent approvals**: when a SubAgent turn requests approval, it creates an awakeable keyed in its own VO state. The approval is routed via the gateway to the parent session's user (a sub-agent does not have its own user). The parent Session VO has a `resolve_child_approval` handler that the gateway calls; it forwards the decision to the child's awakeable via the child's `approve` handler.

---

## State and journal strategy

### Virtual Object state

`Session` VO state (all serde-JSON in RocksDB):

| Key | Type | Purpose |
|---|---|---|
| `meta` | `SessionMeta` | user_id, workspace_id, created_at, model |
| `status` | `SessionStatus` | running / waiting_approval / idle / cancelled |
| `pending` | `Vec<UserMessage>` | queued messages if turn running |
| `pending_approval` | `Option<String>` | awakeable_id if blocked |
| `children` | `Vec<String>` | active sub-agent VO IDs |
| `last_turn_summary` | `Option<String>` | short summary for next turn context |
| `cancel_flag` | `CancelMode` | set by `cancel` handler, read each turn |

`SubAgent` VO state is a superset of Session state with additional keys for `parent_session_id`, `depth`, `budget_remaining`, and `result_awakeable_id`.

Total steady-state per session: ~4–20 KB. A million concurrent sessions fit in ~10 GB of RocksDB, well within a 3-node cluster.

### Journal retention (per handler)

Configured via handler attributes or `restate-server` per-service config:

| Handler | Retention | Rationale |
|---|---|---|
| `Session::post_message` | **48h** | Debug recent sessions without unbounded growth |
| `Session::run_turn` | **24h** | Shorter — inner loop, replayed via parent |
| `Session::approve`, `cancel` | **7d** | Audit who approved what |
| `SubAgent::post_message`, `run_turn` | **24h** | Short-lived, debug recent only |
| `ToolExecutor::execute` | **1h** | Only retained long enough for in-flight retries |
| `LLMGateway::complete` | **1h** | Same |
| `Consolidate::run` | **7d** | Billing/audit |
| `IngestSource::run` | **30d** | Source hash is durable ID; long retention prevents dup-ingestion |
| `MemoryStore::*` | **6h** | Compliance log lives in Postgres, not Restate |

Retention is **completion retention** — the journal is kept for this long *after* the invocation finishes. In-progress invocations retain the journal until completion regardless. A 6-hour session holds its journal for 54 hours total under the 48h setting.

### VO state retention

State persists until explicit deletion. Session and SubAgent VO state is cleared by the `destroy` handler called 72h after completion by a cleanup workflow. Workspace VO state is never auto-cleared (workspaces are permanent). Workflow state auto-clears at retention expiry.

---

## Failure handling and idempotency

### Retry policy

Per-handler, configured in service registration. Defaults:

```toml
[handler.default]
max_attempts = 8
initial_interval_ms = 500
max_interval_ms = 60_000
backoff_multiplier = 2.0

[handler."ToolExecutor/execute"]
max_attempts = 3  # tools fail hard fast; idempotency handles repeats

[handler."LLMGateway/complete"]
max_attempts = 5  # 429s are common, but don't retry forever
```

After `max_attempts`, Restate 1.5's **invocation pause** kicks in — the invocation halts, operator is alerted via OTel span event, and manual resume or cancel happens via the admin API. This gives operators a bounded failure mode instead of silent indefinite retries.

### Idempotency

Tool calls must declare their idempotency class (see `06-hands-and-mcp.md`):
- `Idempotent`: retry freely (file reads, search, most MCP reads)
- `IdempotentWithKey`: retry if idempotency key supported (Stripe, most modern APIs)
- `NonIdempotent`: retry only if no remote side effect confirmed (APIs without idempotency support)

`ToolExecutor::execute` wraps the underlying call in a `ctx.run()` with the idempotency class encoded in the run name. On replay, Restate returns the journaled result; on genuine retry after failure, the tool's idempotency logic kicks in.

### Cancellation

Two paths:
1. **Restate-native**: admin API `cancel_invocation(id)` — terminates the invocation at the next suspension point. Used for hard-stops.
2. **Cooperative**: `Session::cancel` handler sets `cancel_flag` in VO state. `run_turn` checks the flag at each tool-call boundary and returns `TurnOutcome::Cancelled` gracefully. Used for soft-stops where you want the current tool to finish.

---

## Kubernetes deployment

### Cluster topology

Three-node Restate cluster from day 1, no single-node production. The `restate-operator` publishes three CRDs; only the first two are used initially.

```yaml
apiVersion: restate.dev/v1
kind: RestateCluster
metadata:
  name: moa-restate
  namespace: moa-system
spec:
  replicas: 3
  storage:
    storageClassName: fast-ssd
    size: 200Gi
  resources:
    requests: { cpu: 2, memory: 8Gi }
    limits:   { cpu: 4, memory: 16Gi }
  metrics:
    enabled: true
  tracing:
    otlpEndpoint: http://alloy.observability:4317
```

RocksDB storage is local NVMe via `fast-ssd` StorageClass — network-attached volumes tank write latency. Three replicas give you one node loss without quorum loss; raft-style consensus is built in.

### Service registration

MOA services register themselves with Restate at startup via the admin API. The `moa-orchestrator` binary runs as a standard K8s Deployment (not managed by an operator) and exposes its handlers over HTTP/2; Restate server discovers handlers via introspection.

### Autoscaling (Phases 1–2)

Plain HPA on CPU + in-process concurrency limits, no KEDA. In-process concurrency limit is set via Restate handler config: `max_concurrent_invocations: 200` per pod for `Session`, lower for heavy handlers. Prevents new-pod OOM on startup bursts.

### Graceful shutdown

Rust binary handles SIGTERM by deregistering from Restate admin, flipping readiness to False, draining in-flight invocations with a 600s timeout, then exiting. `terminationGracePeriodSeconds: 600`. In-flight invocations that exceed the drain window are transparently reassigned by Restate to other pods — the durable journal makes this safe.

---

## Observability integration

All handler invocations emit OTel spans to Alloy → Tempo. Restate server emits its own spans (invocation lifecycle, journal operations) to the same collector.

Each span carries:
- `restate.invocation_id`, `restate.service`, `restate.handler`, `restate.attempt`
- `moa.tenant_id`, `moa.session_id`, `moa.workspace_id`, `moa.user_id`
- `gen_ai.*` on LLM spans (input_tokens, output_tokens, model, finish_reason)

### Turn-as-trace, session-as-link

Each `post_message` invocation is one trace root. All traces for a session carry `session.id` attribute and a span link to a shared session-root span, reconstructing the full session view in Grafana Tempo's session grouping.

### Dashboards (Grafana)

Four core dashboards land in Phase 1:
1. **Session health**: active sessions, turn p50/p95/p99, approval latency, error rate per tenant tier.
2. **LLM gateway**: tokens/sec per model, 429 rate, cache hit rate, $/min rolling.
3. **Restate internals**: invocation rate per handler, journal size distribution, awakeable-waiting count, retry rate.
4. **Sandbox fleet**: provisioned vs active, provisioning latency p95, hand death rate, idle reaper kills.

---

## Rollout sequencing

Recommended production rollout:

- Deploy `RestateCluster` to the existing Kubernetes cluster.
- Build and register the `moa-orchestrator` binary with all handlers enabled.
- Verify Alloy → Tempo wiring before enabling tenant traffic.
- Run the integration suite against the cluster with synthetic sessions.
- Cut traffic by tenant tier (internal → free → paid → enterprise).
- Keep new-session routing one-way during rollout; do not move in-flight sessions between runtime configurations.

---

## Crate impact

| Crate | Change |
|---|---|
| `moa-orchestrator` | Major rewrite — all Restate handlers live here |
| `moa-session` | Moderate — `SessionStore` trait becomes a Restate Service |
| `moa-brain` | Minimal — pipeline and loop body ported into `Session::run_turn` |
| `moa-hands` | Minimal — exposed via `ToolExecutor` Service |
| `moa-providers` | None — wrapped by `LLMGateway` Service |
| `moa-memory`, `moa-security`, `moa-skills` | None — called as libraries |
| `moa-gateway` | Minor — add awakeable resolution via Restate admin API |
| `moa-core` | Minor — add `invocation_id`, `attempt` fields |
| `moa-restate` (new, optional) | Extract boilerplate if it grows |

---

## Local development

Restate ships a single-binary dev server. `just dev` runs:

```bash
restate-server --node-name local --data-dir .restate-dev &
cargo run -p moa-orchestrator -- --restate http://localhost:9070
restate deployments register http://localhost:9080
```

Integration tests use an in-process test server with tmpdir state. Full session tests complete in seconds locally.

---

## Open decisions

1. **Invocation ID format**: auto-generated UUID with `turn_seq` as span attribute. (leaning)
2. **Awakeable persistence on gateway side**: read-through from Postgres event log, no gateway-side cache. (leaning)
3. **Per-tenant service isolation**: revisit at 10k tenants; single handler pool in Phase 1.
4. **Consolidation scheduling**: `Workspace` VO with delayed self-send. (leaning)
5. **SubAgent result return**: awakeable-based (parent pre-registers). (leaning)

---

## Summary

Restate fits MOA cleanly because three of its primitives match the domain:
- **Virtual Object single-writer semantics** = session and sub-agent serialization for free.
- **Awakeables** = durable approval pauses without gateway-side state.
- **Per-handler retention and invocation pause** = bounded journal growth and human-in-the-loop recovery.

The programming model in Rust is idiomatic. The K8s operator handles the stateful cluster. Observability is OTel-native and lands in Grafana without translation layers.
