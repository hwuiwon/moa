# Step 31: Cooperative Cancellation

## What this step is about
Cancellation is currently task-abort based: `HardCancel` drops the tokio task, and `SoftCancel` sets a flag checked between turns. Neither propagates cancellation to the LLM provider (the HTTP stream keeps running) or to running hand processes (orphaned `docker exec` or future remote hand API calls). This step makes cancellation cooperative across the provider, hand, and orchestrator layers.

## Files to read
- `moa-core/src/types.rs` — `SessionSignal::SoftCancel`, `SessionSignal::HardCancel`, `CompletionStream`
- `moa-core/src/traits.rs` — `HandProvider` trait (has `pause`, `resume`, `destroy` but no `cancel`)
- `moa-orchestrator/src/local.rs` — signal handling for `SoftCancel`/`HardCancel`, `stream_completion_response`
- `moa-providers/src/anthropic.rs` — streaming implementation, completion task
- `moa-providers/src/openai.rs` — streaming implementation
- `moa-providers/src/gemini.rs` — streaming implementation
- `moa-hands/src/local.rs` — `LocalHandProvider`, `execute_docker_tool`, `bash::execute_docker`
- `moa-brain/src/harness.rs` — `stream_completion_response`

## Goal
When a user cancels a session:
1. The LLM provider stream is dropped, and the underlying HTTP connection is closed (not left dangling).
2. Any running hand execution (local process, Docker exec) receives a signal to terminate.
3. The cancellation state is visible to all layers, not just the orchestrator.

## Rules
- Use `tokio_util::sync::CancellationToken` as the shared cooperative cancellation primitive. It's the idiomatic Rust/Tokio pattern — lightweight, cloneable, and composable.
- `SoftCancel` marks the token as cancelled. In-flight work checks the token and finishes its current atomic unit (tool call, etc.) before stopping.
- `HardCancel` marks the token as cancelled AND aborts the completion task's JoinHandle. Running hand processes get a SIGTERM (local) or container stop (Docker).
- Do NOT change the `HandProvider` trait signature. Instead, pass the cancellation token into `execute()` calls via a new field on `ToolContext` or as a separate parameter in the orchestrator's tool dispatch. Hand providers check the token during long-running operations.
- The `CompletionStream` already stops when dropped (receiver closes → sender breaks). Make sure the underlying HTTP client request is also aborted, not just ignored.
- Provider tasks should use `tokio::select!` between the cancellation token and the HTTP stream to respond promptly to cancel.

## Tasks

### 1. Add `CancellationToken` to session state in the orchestrator
Each session in `LocalOrchestrator` gets a `CancellationToken`:
```rust
struct LocalBrainHandle {
    // ... existing fields ...
    cancel_token: CancellationToken,
}
```
On `SoftCancel`: call `cancel_token.cancel()`.
On `HardCancel`: call `cancel_token.cancel()` AND abort the session task.

### 2. Thread the token through the turn engine
The streaming turn loop (in `moa-orchestrator/src/local.rs` or the unified engine from step 30) passes the token to:
- The LLM provider call
- Each tool execution call

### 3. Update `CompletionStream` to accept a cancellation token
Add a method or constructor variant:
```rust
impl CompletionStream {
    /// Aborts the underlying provider task, closing the HTTP connection.
    pub fn abort(&self) {
        self.completion.abort();
    }
}
```
In the streaming loop, when the cancel token fires, call `stream.abort()` and break. This ensures the HTTP request is actually cancelled, not just the receiver dropped.

### 4. Update provider implementations to check cancellation
In each provider's streaming task (`anthropic.rs`, `openai.rs`, `gemini.rs`), add a `tokio::select!` branch:
```rust
tokio::select! {
    chunk = response_stream.next() => {
        // process chunk as before
    }
    _ = cancel_token.cancelled() => {
        tracing::debug!("completion cancelled by user");
        break;
    }
}
```
This requires passing the cancellation token into the `complete()` method. Options:
- Add it to `CompletionRequest`
- Add a `cancel: Option<CancellationToken>` parameter to `LLMProvider::complete()`
- Or keep the current approach where dropping the stream aborts the task — if `JoinHandle::abort()` properly cancels the reqwest future, this may be sufficient

Evaluate which is cleanest. If `JoinHandle::abort()` reliably closes the HTTP connection in reqwest (it does for tokio tasks), then option 3 (no trait change) is sufficient.

### 5. Update local hand execution to respect cancellation
For `LocalHandProvider`:
- **Direct execution**: the spawned `Command` child process can be killed via `child.kill()`. Wrap the process wait in `tokio::select!` with the cancel token.
- **Docker execution**: on cancellation, run `docker stop {container_id} -t 2` (2-second grace period) to terminate the running exec.

```rust
tokio::select! {
    result = child.wait_with_output() => {
        // normal completion
    }
    _ = cancel_token.cancelled() => {
        child.kill().await.ok();
        return Err(MoaError::Cancelled);
    }
}
```

### 6. Add a `MoaError::Cancelled` variant
The error type needs a cancellation variant so call sites can distinguish "operation cancelled by user" from "operation failed":
```rust
pub enum MoaError {
    // ... existing variants ...
    /// Operation was cancelled by the user.
    Cancelled,
}
```
The orchestrator should handle this variant gracefully — emit a `SessionStatusChanged` event, not an `Error` event.

### 7. Update session status transitions
When cancellation completes:
- Emit `Event::SessionStatusChanged { from: Running, to: Cancelled }`
- Do NOT emit `Event::Error` — cancellation is intentional, not a failure
- If a tool was mid-execution, emit `Event::ToolError { error: "cancelled", retryable: false }`

## Deliverables
```
moa-core/src/types.rs              # + CancellationToken on CompletionStream or request
moa-core/src/error.rs              # + MoaError::Cancelled
moa-orchestrator/src/local.rs      # CancellationToken per session, cooperative cancel
moa-providers/src/anthropic.rs     # Cancel-aware streaming (if trait change)
moa-providers/src/openai.rs        # Cancel-aware streaming (if trait change)
moa-providers/src/gemini.rs        # Cancel-aware streaming (if trait change)
moa-hands/src/local.rs             # Process kill on cancel, docker stop on cancel
```

## Acceptance criteria
1. `SoftCancel` stops generation after the current atomic unit (no more tokens stream after cancel).
2. `HardCancel` aborts the LLM stream immediately — the HTTP connection closes.
3. `HardCancel` kills a running local process (`bash` tool) within 2 seconds.
4. `HardCancel` stops a running Docker exec within 2 seconds.
5. Cancellation does not emit `Event::Error` — it emits `SessionStatusChanged` to `Cancelled`.
6. A cancelled `CompletionStream` does not leave the provider task running in the background.
7. All existing tests pass (cancellation is additive).

## Tests

**Unit tests (moa-core):**
- `MoaError::Cancelled` is distinct from other error variants
- `CompletionStream::abort()` stops the completion task

**Unit tests (moa-orchestrator):**
- `SoftCancel` signal → session status becomes `Cancelled` after current turn
- `HardCancel` signal → session status becomes `Cancelled` immediately
- `HardCancel` during LLM streaming → stream stops, no more `AssistantDelta` events
- Cancellation emits `SessionStatusChanged`, not `Error`

**Unit tests (moa-hands):**
- Local process execution with cancel token → process is killed
- Docker exec with cancel token → `docker stop` is invoked

**Integration tests:**
- Start a session → submit a long prompt → `SoftCancel` → verify generation stops cleanly
- Start a session → trigger a `bash("sleep 60")` tool → `HardCancel` → verify the process is killed and session is cancelled within 3 seconds

```bash
cargo test -p moa-core
cargo test -p moa-orchestrator
cargo test -p moa-hands
cargo test -p moa-providers
```
