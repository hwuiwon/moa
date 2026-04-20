# R06 — `Session::run_turn`: Brain Loop

## Purpose

Replace the R05 `run_turn` stub with the real brain loop. This is the largest prompt in the pack: it ports the turn structure from the existing `moa-brain` crate into the Session VO handler, wiring it through the three Services built in R02–R04.

End state: a user message posted to `Session::post_message` drives real LLM completions, real tool executions, and real event logging. Approvals are still deferred (stubs return Deny) — R07 wires them.

## Prerequisites

- R01–R05 complete.
- `moa-brain` crate has a stable `run_turn` function or equivalent that takes a context bundle and returns an LLM response with tool calls.
- `moa-core` context pipeline (from `docs/07-context-pipeline.md`) is callable as a library function.

## Read before starting

- `docs/02-brain-orchestration.md` — the existing brain loop structure
- `docs/07-context-pipeline.md` — context compilation steps
- `docs/12-restate-architecture.md` — the full `run_turn` code sample
- R02's `SessionEvent` types, R03's `LLMGateway::complete`, R04's `ToolExecutor::execute`
- `moa-brain/src/lib.rs` — existing turn loop to port

## Steps

### 1. Extract the reusable brain logic into `moa-brain`

The brain loop today likely lives split between `moa-brain` and `moa-orchestrator` (as older workflow-engine activities). Consolidate into `moa-brain` as a pure library:

```rust
// moa-brain/src/lib.rs
pub struct BrainContext {
    pub system_prompt: String,
    pub messages: Vec<BrainMessage>,
    pub tools: Vec<ToolDescriptor>,
    pub model: String,
    pub max_tokens: usize,
    pub cache_breakpoints: Vec<CacheBreakpoint>,
}

pub struct BrainResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub model: String,
}

pub enum StopReason {
    EndTurn,       // final, no more work
    ToolUse,       // LLM requested tool(s); continue
    MaxTokens,
    Refusal,
}

impl StopReason {
    pub fn is_final(&self) -> bool {
        matches!(self, Self::EndTurn | Self::Refusal | Self::MaxTokens)
    }
}
```

`moa-brain` contains no Restate types. The handler in `moa-orchestrator` consumes these types and invokes the Services.

### 2. Context pipeline as a reusable function

```rust
// moa-brain/src/context.rs
pub async fn build_working_context(
    session_id: Uuid,
    workspace_id: Uuid,
    session_store: &impl SessionReader,  // trait abstraction
    memory_store: &impl MemoryReader,
    skills: &SkillsStore,
) -> Result<BrainContext, BrainError> {
    // 1. Fetch recent events from Postgres (last N turns)
    // 2. Fetch last_turn_summary if present
    // 3. Retrieve relevant memory pages (RAG)
    // 4. Compile skills scoped to workspace + query
    // 5. Assemble system prompt, messages, tools, cache breakpoints
    // 6. Return BrainContext
}
```

`SessionReader` / `MemoryReader` are thin traits that the orchestrator implements by calling `SessionStore` / `MemoryStore` Services. This keeps `moa-brain` independent of Restate.

### 3. Replace the `run_turn` stub

`moa-orchestrator/src/objects/session.rs`:

```rust
async fn run_turn(ctx: ObjectContext<'_>) -> Result<TurnOutcome, HandlerError> {
    let session_id: uuid::Uuid = ctx.key().parse()?;
    let meta = ctx.get::<SessionMeta>(K_META).await?
        .ok_or_else(|| HandlerError::from("session meta missing"))?;

    // Cooperative cancellation check at turn boundary.
    if let Some(mode) = ctx.get::<CancelMode>(K_CANCEL_FLAG).await? {
        tracing::info!(?mode, "turn cancelled before start");
        return Ok(TurnOutcome::Cancelled);
    }

    // Pop pending messages (if any) and consume them for this turn.
    let pending = ctx.get::<Vec<UserMessage>>(K_PENDING).await?.unwrap_or_default();
    if pending.is_empty() {
        return Ok(TurnOutcome::Idle);
    }
    ctx.set(K_PENDING, Vec::<UserMessage>::new());

    // Build context via the brain library. The context call is itself a side effect
    // (reads Postgres, embeddings, etc.) so wrap in ctx.run for determinism.
    let ctx_bundle = ctx.run("build_context", || async {
        build_working_context_via_services(&ctx, session_id, meta.workspace_id).await
    }).await?;

    // Call LLMGateway.
    let mut llm_req = CompletionRequest {
        model: meta.model.clone(),
        system: ctx_bundle.system_prompt,
        messages: ctx_bundle.messages,
        tools: ctx_bundle.tools,
        max_tokens: ctx_bundle.max_tokens,
        session_id: Some(session_id),
        cache_breakpoints: ctx_bundle.cache_breakpoints,
    };

    // Append pending user messages to the last assistant message (context is already
    // "up to the last assistant turn"; new user messages come after).
    for user_msg in pending {
        llm_req.messages.push(BrainMessage::User(user_msg));
    }

    let response = ctx.service_client::<LLMGatewayClient>()
        .complete(llm_req)
        .call()
        .await?;

    // 3. Handle tool calls.
    for tool_call in response.tool_calls.iter() {
        // Cancellation check at each tool boundary (soft-cancel semantics).
        if let Some(_mode) = ctx.get::<CancelMode>(K_CANCEL_FLAG).await? {
            return Ok(TurnOutcome::Cancelled);
        }

        // Log the call before dispatching (so if crash happens between, we see it).
        ctx.service_client::<SessionStoreClient>()
            .append_event(session_id, SessionEvent::ToolCall {
                tool_id: tool_call.id,
                tool_name: tool_call.name.clone(),
                input: tool_call.input.clone(),
            })
            .call()
            .await?;

        // Approval: R07 expands this. For R06, auto-deny approval-required tools.
        if requires_approval(&tool_call.name, &meta.workspace_id) {
            ctx.service_client::<SessionStoreClient>()
                .append_event(session_id, SessionEvent::Error {
                    message: format!("Tool {} requires approval (not yet implemented in R06)", tool_call.name),
                    recoverable: true,
                })
                .call()
                .await?;
            continue;
        }

        let tool_req = ToolCallRequest {
            tool_call_id: tool_call.id,
            tool_name: tool_call.name.clone(),
            input: tool_call.input.clone(),
            session_id: Some(session_id),
            workspace_id: meta.workspace_id,
            tenant_id: meta.tenant_id,
            idempotency_key: tool_call.idempotency_key.clone(),
        };

        let _tool_output = ctx.service_client::<ToolExecutorClient>()
            .execute(tool_req)
            .call()
            .await?;
        // ToolExecutor already emits ToolResult events; don't duplicate here.
    }

    // Update last_turn_summary for next context build.
    if let Some(summary) = response.turn_summary.clone() {
        ctx.set(K_LAST_TURN_SUMMARY, summary);
    }

    // If stop_reason is final and no tool calls, turn is done.
    // If stop_reason is tool_use, continue for another turn (tool results feed back).
    Ok(if response.stop_reason.is_final() && response.tool_calls.is_empty() {
        TurnOutcome::Idle
    } else {
        TurnOutcome::Continue
    })
}
```

### 4. Bridge `moa-brain`'s context readers to Restate services

```rust
// moa-orchestrator/src/brain_bridge.rs
pub struct SessionReaderOverRestate<'a, 'b> {
    pub ctx: &'a ObjectContext<'b>,
}

impl<'a, 'b> moa_brain::SessionReader for SessionReaderOverRestate<'a, 'b> {
    async fn get_recent_events(&self, session_id: Uuid, n: u32)
        -> Result<Vec<EventRecord>, BrainError>
    {
        self.ctx.service_client::<SessionStoreClient>()
            .get_events(session_id, EventRange { limit: Some(n), ..Default::default() })
            .call()
            .await
            .map_err(|e| BrainError::Io(e.to_string()))
    }

    async fn get_session(&self, session_id: Uuid) -> Result<SessionMeta, BrainError> {
        self.ctx.service_client::<SessionStoreClient>()
            .get_session(session_id)
            .call()
            .await
            .map_err(|e| BrainError::Io(e.to_string()))
    }
}
```

Same pattern for `MemoryReader` (calls `MemoryStore` Service — stubbed for R06 if not yet built).

### 5. Turn loop termination conditions

The turn loop in `post_message` (from R05) drives turns until one of these conditions:
- `TurnOutcome::Idle` — no pending work, LLM said it's done
- `TurnOutcome::WaitingApproval` — paused on awakeable (R07)
- `TurnOutcome::Cancelled` — cancel flag set
- Or an unrecoverable error propagates

Max turn cap: enforce a per-session budget to prevent runaway loops. Add a counter to VO state:

```rust
const K_TURN_COUNT: &str = "turn_count";
const MAX_TURNS_PER_POST: usize = 50;

// In post_message loop:
let mut turns_this_post = 0;
loop {
    turns_this_post += 1;
    if turns_this_post > MAX_TURNS_PER_POST {
        ctx.service_client::<SessionStoreClient>()
            .append_event(session_id, SessionEvent::Error {
                message: format!("Turn budget exceeded ({}), stopping", MAX_TURNS_PER_POST),
                recoverable: true,
            })
            .call()
            .await?;
        break;
    }
    // ... run_turn
}
```

### 6. Streaming deferred

R06 uses non-streaming `LLMGateway::complete`. The user-visible streaming path (for gateway UI updates) is enabled later: once the full response is computed, `BrainResponse` is emitted in chunks to the session event log, and the gateway subscribes to the log. Real provider-side streaming requires `stream_complete` + `poll_stream` which are stubs from R03. Defer until after R09.

### 7. Unit tests

`moa-orchestrator/tests/session_run_turn.rs`:

- `run_turn_without_pending_returns_idle` — no pending messages → Idle
- `run_turn_calls_llm_with_context` — mock LLMGateway, assert called with expected request
- `run_turn_executes_tool_calls_in_order` — mock response with 3 tool calls, assert ToolExecutor called 3 times
- `run_turn_checks_cancel_between_tools` — set cancel flag after first tool, assert second tool not called
- `run_turn_logs_all_events` — assert ToolCall events appear in SessionStore in order
- `run_turn_budget_exceeded` — 50+ Continue outcomes → error event + break

### 8. Integration test

`moa-orchestrator/tests/integration/session_brain_e2e.rs`:

- Create session via `SessionStore/create_session` + `init_session_vo`.
- Post a simple message: "What is 2+2? Just answer."
- Assert session completes in Idle status.
- Assert events: UserMessage → BrainResponse → (final status update).
- Post a tool-requiring message: "Read the file at /tmp/test.txt" (pre-create the file).
- Assert events: UserMessage → BrainResponse → ToolCall → ToolResult → BrainResponse → Idle.

Requires `ANTHROPIC_API_KEY` (or similar); mark `#[ignore]` for CI without secrets.

## Files to create or modify

- `moa-brain/src/lib.rs` — expose `BrainContext`, `BrainResponse`, `StopReason`
- `moa-brain/src/context.rs` — `build_working_context` function, `SessionReader`/`MemoryReader` traits
- `moa-orchestrator/src/objects/session.rs` — replace `run_turn` stub
- `moa-orchestrator/src/brain_bridge.rs` — new, trait impls over Restate services
- `moa-core/src/types.rs` — `CompletionRequest`, `BrainMessage`, `ToolCall`, `StopReason` if not already present
- `moa-orchestrator/tests/session_run_turn.rs` — unit tests
- `moa-orchestrator/tests/integration/session_brain_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` and `-p moa-brain` succeed.
- [ ] All unit tests pass.
- [ ] Integration test passes end-to-end with a real provider: 2+2 session completes; file-read session completes with correct tool invocation.
- [ ] Event log in Postgres shows the full sequence: UserMessage, BrainResponse, ToolCall, ToolResult, BrainResponse.
- [ ] Pod restart mid-turn: the turn replays from the journal, no duplicate LLM calls, no duplicate tool invocations.
- [ ] Soft cancel during tool execution: current tool completes, next tool is skipped, status → Cancelled.
- [ ] Max turn budget enforced: a session that loops >50 turns halts with an Error event.

## Notes

- **LLM calls replay from journal**, not from the provider, on retry. Verify this by watching the provider's request count during a forced pod kill — it should not double.
- **Tool call ordering**: the LLM may return multiple tool calls in a single response (parallel tool use). Execute them sequentially in the journaled order for replay determinism. Parallel execution can be added later via `ctx.run()` with gathered futures, but it complicates replay ordering.
- **Context rebuild each turn**: yes, the context is rebuilt from Postgres on each turn rather than threaded through state. This is simpler and the context pipeline is cheap relative to the LLM call. Future optimization: cache the assembled context in VO state and invalidate on specific event types.
- **Do not emit BrainResponse event**: `LLMGateway::complete` already emits it. Emit `ToolCall` from the Session VO before dispatching; `ToolResult` is emitted by `ToolExecutor::execute`.
- **`requires_approval`** is a placeholder function that reads workspace policy. R07 makes this real; for R06, hardcode to return `false` for all built-in tools.

## What R07 expects

- Brain loop executes real LLM + tool calls.
- Event log shows full turn sequences.
- The approval stub exists at the right point in the loop — R07 replaces it with awakeables.
- Turn outcome `WaitingApproval` is wired through `post_message` but never returned by R06.
