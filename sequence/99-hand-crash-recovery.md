# 99 — Hand Crash Recovery & Error Classification

## Purpose

Add structured error classification and re-provision logic to the `ToolExecutor` Restate service so that sandbox failures (Daytona timeout, E2B OOM, Docker exit, MCP disconnect) are handled correctly rather than propagating as opaque errors. The brain should never see a raw infrastructure failure — it should see either a clean retry result or a classified terminal error with actionable context.

End state: `ToolExecutor::execute` classifies every error into `Retryable`, `ReProvision`, or `Fatal`; re-provisions sandboxes automatically on sandbox-death signals; surfaces clean error messages to the brain; and emits structured telemetry for each failure class.

## Prerequisites

- R04 (`ToolExecutor` service) complete and working.
- `moa-hands/src/daytona.rs`, `moa-hands/src/e2b.rs`, `moa-hands/src/local.rs` all functional.
- Sequence 66 (`retry-module-gemini`) landed — reuse the retry infrastructure.

## Read before starting

```
cat moa-hands/src/daytona.rs
cat moa-hands/src/e2b.rs
cat moa-hands/src/local.rs
cat moa-hands/src/router/mod.rs
cat moa-orchestrator/src/services/tool_executor.rs
cat moa-orchestrator/src/turn/runner.rs
cat moa-core/src/error.rs
```

## Architecture

### Error classification enum

```rust
/// Classification of tool execution failures for retry/recovery decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFailureClass {
    /// Transient infrastructure error — retry in place with backoff.
    /// Examples: 429, 503, network timeout on a healthy sandbox.
    Retryable {
        reason: String,
        backoff_hint: Duration,
    },
    /// Sandbox is dead or unresponsive — destroy and re-provision before retry.
    /// Examples: E2B TimeoutException(unavailable), Docker exited, Daytona 502.
    ReProvision {
        reason: String,
    },
    /// Permanent failure — do not retry, surface to brain immediately.
    /// Examples: invalid tool name, auth failure, policy denial, schema error.
    Fatal {
        reason: String,
    },
}
```

### Classification rules

| Signal | Classification |
|---|---|
| HTTP 429 / rate limit | `Retryable(backoff: Retry-After header or 2s)` |
| HTTP 502/503/504 | `Retryable(backoff: 1s)` first attempt, `ReProvision` after 2nd |
| HTTP 401/403 | `Fatal` |
| HTTP 400 (bad request) | `Fatal` |
| E2B `TimeoutException(unavailable\|unknown)` | `ReProvision` |
| E2B `TimeoutException(deadline_exceeded)` | `Retryable(backoff: 0)` — command was slow, not sandbox death |
| Daytona sandbox status `Error` or `Stopped` | `ReProvision` |
| Docker container `Status=exited` with non-zero exit | `ReProvision` |
| Docker `ConnectionRefused` / socket error | `ReProvision` |
| Two consecutive exec timeouts on same sandbox | `ReProvision` (swap-thrashing) |
| Tool not found in registry | `Fatal` |
| JSON schema validation failure on input | `Fatal` |
| MCP server disconnect | `ReProvision` (reconnect MCP) |
| Network timeout (single occurrence) | `Retryable(backoff: 1s)` |

### Recovery flow

```
1. ToolExecutor::execute receives tool call
2. Attempt execution via HandProvider
3. On error → classify(error) → match class:
   a. Fatal → return ToolOutput::error immediately
   b. Retryable → sleep(backoff), retry up to 3 times
   c. ReProvision → destroy old sandbox, provision new one, retry once
4. On success → return ToolOutput
5. Re-provision cap: max 2 re-provisions per session
6. Emit metrics: moa.tool.failure{class=retryable|reprovision|fatal}
```

## Steps

### 1. Add `ToolFailureClass` to `moa-core/src/error.rs`

Add the enum above. Add a `classify_tool_error(error: &MoaError, consecutive_timeouts: u32) -> ToolFailureClass` function that inspects error variants and HTTP status codes. Add `impl From<ToolFailureClass> for ToolOutput` that produces a well-formatted error message for the brain.

### 2. Add `classify` method to each hand provider

In `moa-hands/src/daytona.rs`, `e2b.rs`, `local.rs`, and `mcp.rs`, add a method:

```rust
pub fn classify_error(error: &HandError) -> ToolFailureClass
```

Each provider knows its own error shapes. The Daytona classifier inspects HTTP status codes and sandbox state. The E2B classifier inspects `TimeoutException` subtypes. The local classifier inspects process exit codes and Docker container inspect results. The MCP classifier inspects transport errors.

### 3. Add retry/re-provision loop to `ToolRouter::execute`

In `moa-hands/src/router/mod.rs`, wrap the existing `execute` call in a retry loop:

```rust
pub async fn execute_with_recovery(
    &self,
    request: &ToolCallRequest,
    session_ctx: &SessionContext,
) -> ToolOutput {
    let mut attempts = 0;
    let mut reprovisions = 0;
    let mut consecutive_timeouts = 0;

    loop {
        attempts += 1;
        match self.execute_inner(request, session_ctx).await {
            Ok(output) => return output,
            Err(error) => {
                let class = self.classify(&error, consecutive_timeouts);
                match class {
                    Fatal { reason } => return ToolOutput::error(reason, Duration::ZERO),
                    Retryable { backoff_hint, .. } if attempts < 3 => {
                        tokio::time::sleep(backoff_hint).await;
                        continue;
                    }
                    ReProvision { .. } if reprovisions < 2 => {
                        self.destroy_and_reprovision(session_ctx).await;
                        reprovisions += 1;
                        consecutive_timeouts = 0;
                        continue;
                    }
                    _ => return ToolOutput::error(
                        format!("tool execution failed after {attempts} attempts: {error}"),
                        Duration::ZERO,
                    ),
                }
            }
        }
    }
}
```

### 4. Wire into `ToolExecutorImpl` Restate service

In `moa-orchestrator/src/services/tool_executor.rs`, call `execute_with_recovery` instead of the bare `execute`. The Restate service itself should NOT retry (let the inner loop handle it) — Restate's journal ensures the entire execute-with-recovery block is replayed on process crash.

### 5. Add health probe for active sandboxes

Add `HandProvider::health_check(&self, handle: &HandHandle) -> Result<bool>` to each provider. Before executing a tool call, check if the sandbox is alive. If not, re-provision proactively instead of discovering the failure during execution. This is optional — the reactive recovery above is sufficient, but proactive health checks reduce user-visible latency.

### 6. Metrics and tracing

Emit counters:
- `moa_tool_failure_total{class="retryable|reprovision|fatal", provider="daytona|e2b|local|mcp", tool="bash|file_read|..."}`
- `moa_tool_reprovision_total{provider="..."}`
- `moa_tool_retry_total{attempt="1|2|3", provider="..."}`

Emit tracing spans around each retry and re-provision with the error classification attached.

### 7. Tests

- Unit: `classify_error` returns correct class for each error variant
- Unit: retry loop stops after 3 retries for `Retryable`
- Unit: retry loop re-provisions and retries once for `ReProvision`
- Unit: retry loop returns immediately for `Fatal`
- Unit: re-provision cap at 2 per session
- Integration: mock a Daytona 502 on first attempt, success on retry → tool output is success
- Integration: mock E2B unavailable → re-provision → success

## Files to create or modify

- `moa-core/src/error.rs` — add `ToolFailureClass`, `classify_tool_error`
- `moa-hands/src/daytona.rs` — add `classify_error`
- `moa-hands/src/e2b.rs` — add `classify_error`
- `moa-hands/src/local.rs` — add `classify_error`
- `moa-hands/src/mcp.rs` — add `classify_error`
- `moa-hands/src/router/mod.rs` — add `execute_with_recovery`, wire classification
- `moa-orchestrator/src/services/tool_executor.rs` — call `execute_with_recovery`
- `moa-core/src/metrics.rs` — add failure/retry/reprovision counters

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] `cargo test -p moa-hands` — all classification and retry tests pass.
- [ ] A Daytona 502 on first attempt is retried and succeeds on second attempt.
- [ ] An E2B `unavailable` triggers re-provision → success.
- [ ] A fatal error (401, unknown tool) returns immediately without retry.
- [ ] Metrics counters are emitted for each failure class.
- [ ] Re-provision capped at 2 per session.