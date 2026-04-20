# R04 — `ToolExecutor` Service

## Purpose

Ship the `ToolExecutor` Service: a Restate Service that wraps `moa-hands` and enforces the idempotency contract for tool calls. This prompt introduces the idempotency-class-aware retry wrapper, which is critical for correctness — a tool that isn't idempotent must not silently retry and cause duplicate side effects.

End state: `ToolExecutor::execute` accepts a `ToolCallRequest`, resolves the target hand, executes the tool, records the result, and handles failures per the tool's declared idempotency class. Tool registry integration works for built-in tools (bash, file_read, file_write, file_search, web_search, web_fetch, memory_search, memory_write).

## Prerequisites

- R01, R02, R03 complete.
- `moa-hands` crate exists with `HandProvider` trait and at least `LocalHandProvider` implementation.
- `moa-core` has `Tool`, `ToolRegistry`, and `IdempotencyClass` types.

## Read before starting

- `docs/06-hands-and-mcp.md` — full hand model, tool routing, idempotency classes
- `docs/12-restate-architecture.md` — "Idempotency" section
- `moa-hands/src/lib.rs` — existing `HandProvider` and `ToolRouter`
- `moa-core/src/types.rs` — `Tool`, `ToolCallRequest`, `ToolOutput`

## Steps

### 1. Formalize the idempotency contract

`moa-core/src/types.rs` — ensure the following exist (add if missing):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum IdempotencyClass {
    /// Safe to retry freely. Result is deterministic per input.
    /// Examples: file_read, memory_search, web_search.
    Idempotent,

    /// Safe to retry *if* an idempotency key is supplied.
    /// Examples: Stripe calls, most modern REST APIs, HTTP PUT.
    IdempotentWithKey,

    /// Retry only if we can verify no side effect occurred.
    /// Examples: shell commands, APIs without idempotency support.
    NonIdempotent,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCallRequest {
    pub tool_call_id: uuid::Uuid,
    pub tool_name: String,
    pub input: serde_json::Value,
    pub session_id: Option<uuid::Uuid>,
    pub workspace_id: uuid::Uuid,
    pub tenant_id: uuid::Uuid,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolOutput {
    pub tool_call_id: uuid::Uuid,
    pub output: String,
    pub success: bool,
    pub duration_ms: u64,
    pub truncated: bool,
}

pub trait Tool {
    fn name(&self) -> &str;
    fn idempotency_class(&self) -> IdempotencyClass;
    fn schema(&self) -> serde_json::Value;
}
```

### 2. Define the Service trait

`moa-orchestrator/src/services/tool_executor.rs`:

```rust
use restate_sdk::prelude::*;
use moa_core::types::*;

#[restate_sdk::service]
pub trait ToolExecutor {
    async fn execute(
        ctx: Context<'_>,
        req: ToolCallRequest,
    ) -> Result<ToolOutput, HandlerError>;

    /// Introspect available tools for the given workspace.
    async fn list_tools(
        ctx: Context<'_>,
        workspace_id: uuid::Uuid,
    ) -> Result<Vec<ToolDescriptor>, HandlerError>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub idempotency_class: IdempotencyClass,
    pub requires_approval: bool,
}
```

### 3. Implement `execute` with idempotency-aware wrapping

```rust
use std::sync::Arc;
use moa_hands::{HandProvider, ToolRouter};

pub struct ToolExecutorImpl {
    pub router: Arc<ToolRouter>,
}

impl ToolExecutor for ToolExecutorImpl {
    async fn execute(
        ctx: Context<'_>,
        req: ToolCallRequest,
    ) -> Result<ToolOutput, HandlerError> {
        let router = get_router_from_ctx(&ctx);
        let tool = router.resolve(&req.tool_name)
            .ok_or_else(|| HandlerError::from(format!("Unknown tool: {}", req.tool_name)))?;

        // The ctx.run name encodes the tool_call_id so replays hit the exact journal entry.
        // Idempotency class determines whether to retry on transient errors.
        let run_name = match tool.idempotency_class() {
            IdempotencyClass::Idempotent =>
                format!("tool_exec_idempotent_{}", req.tool_call_id),
            IdempotencyClass::IdempotentWithKey => {
                let key = req.idempotency_key.as_deref().ok_or_else(|| {
                    HandlerError::from("IdempotentWithKey tool requires idempotency_key")
                })?;
                format!("tool_exec_keyed_{}_{}", req.tool_call_id, key)
            }
            IdempotencyClass::NonIdempotent =>
                format!("tool_exec_nonidem_{}", req.tool_call_id),
        };

        let req_clone = req.clone();
        let result: Result<ToolOutput, String> = ctx
            .run(&run_name, || async move {
                let start = std::time::Instant::now();
                let raw = router.execute(&req_clone).await
                    .map_err(|e| e.to_string())?;
                Ok(ToolOutput {
                    tool_call_id: req_clone.tool_call_id,
                    output: raw.output,
                    success: raw.success,
                    duration_ms: start.elapsed().as_millis() as u64,
                    truncated: raw.truncated,
                })
            })
            .await?;

        let output = result.map_err(HandlerError::from)?;

        // Emit event.
        if let Some(session_id) = req.session_id {
            ctx.service_client::<SessionStoreClient>()
                .append_event(session_id, SessionEvent::ToolResult {
                    tool_id: output.tool_call_id,
                    output: output.output.clone(),
                    success: output.success,
                    duration_ms: output.duration_ms,
                })
                .send();
        }

        Ok(output)
    }

    async fn list_tools(
        ctx: Context<'_>,
        workspace_id: uuid::Uuid,
    ) -> Result<Vec<ToolDescriptor>, HandlerError> {
        let router = get_router_from_ctx(&ctx);
        let tools = router.list_for_workspace(workspace_id);
        Ok(tools.into_iter().map(|t| ToolDescriptor {
            name: t.name().to_string(),
            description: t.description().to_string(),
            schema: t.schema(),
            idempotency_class: t.idempotency_class(),
            requires_approval: t.requires_approval(),
        }).collect())
    }
}
```

### 4. NonIdempotent safety check

For `NonIdempotent` tools, a naive retry is dangerous: the tool may have already committed a side effect before crashing. The safety pattern is **check-before-retry via the event log**:

```rust
// Inside the ctx.run for NonIdempotent tools:
if matches!(tool.idempotency_class(), IdempotencyClass::NonIdempotent) {
    // Before executing, check: has a prior attempt already recorded a result for this tool_call_id?
    let prior_results = ctx.service_client::<SessionStoreClient>()
        .get_events_by_type(
            req.session_id.ok_or_else(|| HandlerError::from("session_id required for NonIdempotent"))?,
            "ToolResult".to_string(),
        )
        .call()
        .await?;

    if prior_results.iter().any(|e| event_matches_tool_id(e, req.tool_call_id)) {
        return Err(HandlerError::from(format!(
            "Refusing to re-execute NonIdempotent tool {} (tool_call_id={}) — prior attempt logged a result. Propagate failure to LLM.",
            req.tool_name, req.tool_call_id
        )));
    }
}
```

This is defense-in-depth: Restate's own journal should prevent re-execution on the same invocation, but if the invocation crashed *after* the tool executed but *before* the journal entry was written, the subsequent retry sees the event log record and refuses.

### 5. Retry configuration

```rust
#[restate_sdk::service]
pub trait ToolExecutor {
    #[retry(max_attempts = 3, initial_interval_ms = 500)]
    async fn execute(ctx: Context<'_>, req: ToolCallRequest)
        -> Result<ToolOutput, HandlerError>;
    // ...
}
```

3 attempts total is intentional — tools either succeed fast or fail hard. The idempotency class handles what happens on each attempt. Exceeding 3 attempts hits Restate's invocation pause.

### 6. Wire into main

```rust
use moa_hands::{LocalHandProvider, ToolRouter};

let hand_provider = Arc::new(LocalHandProvider::new(/* config */));
let router = Arc::new(ToolRouter::new(hand_provider));

HttpServer::new(
    Endpoint::builder()
        .bind(services::health::HealthImpl.serve())
        .bind(services::session_store::SessionStoreImpl { pool: pool.clone() }.serve())
        .bind(services::llm_gateway::LLMGatewayImpl { providers: providers.clone() }.serve())
        .bind(services::tool_executor::ToolExecutorImpl { router: router.clone() }.serve())
        .build(),
)
.listen_and_serve(...)
.await
```

### 7. Unit tests

`moa-orchestrator/tests/tool_executor.rs`:

- `idempotent_tool_retries_freely` — mock tool that fails twice then succeeds; expect 3 attempts, success
- `non_idempotent_refuses_after_event_log_hit` — mock a tool_call_id that has a prior ToolResult event; expect refusal
- `keyed_tool_requires_idempotency_key` — IdempotentWithKey tool without key → HandlerError
- `run_name_encodes_tool_call_id` — assert run name format for replay stability
- `list_tools_returns_workspace_tools` — registry returns correct filtered list

### 8. Integration test

`moa-orchestrator/tests/integration/tool_executor_e2e.rs`:

- Register service.
- Call `ToolExecutor/execute` with `file_read` (Idempotent) on a test file, assert output contains file content.
- Call with `bash` (NonIdempotent) running `echo hello`, assert output = "hello\n".
- Attempt second `bash` call with same tool_call_id — assert refusal.
- Call `ToolExecutor/list_tools` — assert returns at least `bash`, `file_read`, `file_write`.

## Files to create or modify

- `moa-core/src/types.rs` — add/confirm `IdempotencyClass`, `ToolCallRequest`, `ToolOutput`, `Tool` trait extensions
- `moa-orchestrator/src/services/tool_executor.rs` — new
- `moa-orchestrator/src/services/mod.rs` — add `pub mod tool_executor;`
- `moa-orchestrator/src/main.rs` — wire service
- `moa-orchestrator/Cargo.toml` — add `moa-hands` dep
- `moa-hands/src/lib.rs` — ensure `ToolRouter::execute` returns a type compatible with `ToolOutput`; add `idempotency_class()` on each tool if not already present
- `moa-orchestrator/tests/tool_executor.rs` — unit tests
- `moa-orchestrator/tests/integration/tool_executor_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` succeeds.
- [ ] `cargo test -p moa-orchestrator tool_executor` passes.
- [ ] Every tool in the registry declares an `IdempotencyClass` — no defaults, must be explicit.
- [ ] `restate invocation call 'ToolExecutor/execute'` with a `file_read` request returns the file content.
- [ ] `restate invocation call 'ToolExecutor/execute'` with a `bash` request running a simple command returns the output.
- [ ] Re-invoking the same `tool_call_id` for a NonIdempotent tool returns an error mentioning prior result.
- [ ] `ToolResult` events are written to Postgres after successful executions.

## Notes

- **Idempotency is the author's responsibility**, not the executor's. This service cannot make a non-idempotent tool safe; it can only prevent double-execution visible to the event log. Tools must correctly self-classify.
- **MCP tools are deferred**: this prompt handles built-in tools and local/Daytona hand execution. MCP server integration with the credential proxy lands in Phase 3.
- **Approval check is not here**: the question "does this tool require approval?" lives in the Session VO's run_turn loop (R06, R07). ToolExecutor is called only after approval is granted.
- **Result truncation**: tool outputs over ~32KB should be truncated and summarized for context; full output stays in Postgres. For R04, truncate at a conservative 64KB and set `truncated: true`; brain loop consumes the shorter version.
- **Timeouts are per-tool**: pass through to the hand provider. Default 5 minutes for bash, 30s for most others. This lives in tool metadata, not in the executor.

## What R05 expects

- `ToolExecutor::execute` callable from VO handlers.
- Idempotency classes enforced.
- Tool registry populated with built-in tools.
- Result events persisted to Postgres.
