# Step 30: Unified Streaming Turn Engine

## What this step is about
The brain turn logic currently exists in two places:
1. `moa-brain::run_brain_turn_with_tools()` — the canonical buffered harness that emits final `Event::BrainResponse` events after collecting the full response.
2. `moa-tui::runner` (LocalChatRuntime / DaemonChatRuntime) — a separate streaming implementation that drives the compile → stream → tool/approval → continue loop, emitting `RuntimeEvent` deltas (character-by-character assistant text, tool status updates, approval prompts).

Both paths reimplement overlapping orchestration logic. Changes to turn execution need to be kept in sync across both. Additionally, `RuntimeEvent` observation is exposed only through `LocalOrchestrator::observe_runtime()` — a concrete method, not part of the `BrainOrchestrator` trait. Remote/daemon clients need a transport-level equivalent.

This step extracts a single streaming turn engine that both the buffered harness and the TUI consume, and promotes runtime observation into the orchestrator trait.

## Files to read
- `moa-brain/src/harness.rs` — `run_brain_turn_with_tools()`, the buffered turn loop
- `moa-tui/src/runner.rs` — `LocalChatRuntime`, `DaemonChatRuntime`, streaming turn loop, `run_streamed_turn()`
- `moa-orchestrator/src/local.rs` — `LocalOrchestrator`, `observe_runtime()`, `run_brain_session()`, `run_single_turn()`
- `moa-core/src/types.rs` — `RuntimeEvent` enum
- `moa-core/src/traits.rs` — `BrainOrchestrator` trait (has `observe()` for `EventStream`, no `observe_runtime()`)
- `moa-cli/src/exec.rs` — how `moa exec` drives the runtime

## Goal
One streaming turn engine in `moa-brain` (or `moa-orchestrator`) that produces `RuntimeEvent`s as the primary output. Buffered turn results are derived from the stream (collect until `TurnCompleted`). The `BrainOrchestrator` trait gains a runtime observation method. TUI and CLI become pure consumers of the stream.

## Rules
- The streaming engine is the **single source of truth** for turn execution. The buffered path should wrap it, not duplicate it.
- `RuntimeEvent` moves conceptually from a "TUI concern" to a "framework-level concern". It stays in `moa-core/src/types.rs` (where it already is).
- The turn engine must emit both `RuntimeEvent`s (for live observation) and persist `Event`s (for the durable log). These are distinct: `RuntimeEvent::AssistantDelta` is ephemeral (not persisted), while `Event::BrainResponse` is durable (persisted). The engine writes to both a `broadcast::Sender<RuntimeEvent>` and the `SessionStore`.
- The `BrainOrchestrator` trait gains `observe_runtime()` returning a receiver of `RuntimeEvent`s. The local implementation uses `broadcast::Receiver`. The cloud/daemon implementation can use SSE or WebSocket transport later.
- The TUI's `LocalChatRuntime` should be simplified to a thin wrapper that calls the orchestrator and consumes the stream — it should NOT contain turn execution logic.
- The `DaemonChatRuntime` already consumes events over IPC; this step does not change its transport, only ensures the event types align.

## Tasks

### 1. Add `observe_runtime()` to `BrainOrchestrator` trait in `moa-core/src/traits.rs`
```rust
#[async_trait]
pub trait BrainOrchestrator: Send + Sync {
    // ... existing methods ...

    /// Subscribes to live runtime events for a session (streaming deltas, tool updates, approvals).
    /// Returns None if the session is not running or observation is not supported.
    async fn observe_runtime(
        &self,
        session_id: SessionId,
    ) -> Result<Option<broadcast::Receiver<RuntimeEvent>>>;
}
```

### 2. Extract the streaming turn engine into `moa-orchestrator` (or `moa-brain`)
The turn execution logic currently split between `harness.rs` and `runner.rs` should be unified into a single `run_streamed_turn()` function that:

1. Compiles context through the pipeline
2. Calls the LLM provider with streaming enabled
3. For each streamed token: emits `RuntimeEvent::AssistantDelta` to the broadcast channel
4. On stream completion: emits `RuntimeEvent::AssistantFinished` and persists `Event::BrainResponse`
5. For tool calls: emits `RuntimeEvent::ToolUpdate` (status changes), checks policy, handles approval
6. For approval needed: emits `RuntimeEvent::ApprovalRequested`, persists `Event::ApprovalRequested`
7. Returns `TurnResult` (Continue, Complete, NeedsApproval, Error)

This function takes:
- `session_id`, `session_store`, `memory_store`, `llm_provider`, `tool_router`, `pipeline`
- `runtime_tx: broadcast::Sender<RuntimeEvent>` — the observation channel

### 3. Make `run_brain_turn_with_tools()` a wrapper
The existing buffered harness becomes:
```rust
pub async fn run_brain_turn_with_tools(...) -> Result<TurnResult> {
    let (runtime_tx, _) = broadcast::channel(256); // no observer needed
    run_streamed_turn(..., runtime_tx).await
}
```
This preserves backward compat for callers that don't need streaming.

### 4. Update `LocalOrchestrator::run_single_turn()`
Replace the current turn execution with a call to the unified `run_streamed_turn()`, passing the session's `runtime_tx`. Remove duplicated orchestration logic.

### 5. Implement `observe_runtime()` on `LocalOrchestrator`
This already exists as a concrete method. Promote it to the trait impl:
```rust
async fn observe_runtime(&self, session_id: SessionId) -> Result<Option<broadcast::Receiver<RuntimeEvent>>> {
    let sessions = self.sessions.read().await;
    let handle = sessions.get(&session_id)
        .ok_or(MoaError::SessionNotFound(session_id))?;
    Ok(Some(handle.runtime_tx.subscribe()))
}
```

### 6. Implement `observe_runtime()` on the cloud runtime
Return `Ok(None)` for now — the cloud runtime does not have local broadcast channels. Add a `// TODO: implement via SSE/WebSocket transport` comment. The trait returns `Option` specifically to handle this case.

### 7. Simplify `LocalChatRuntime` in `moa-tui/src/runner.rs`
Remove the turn execution logic from the TUI runtime. Replace with:
1. Call `orchestrator.start_session()` or `orchestrator.signal(QueueMessage)`
2. Subscribe via `orchestrator.observe_runtime(session_id)`
3. Forward `RuntimeEvent`s to the TUI event channel
4. Handle approval decisions by calling `orchestrator.signal(ApprovalDecided)`

The TUI runtime becomes a pure consumer — no pipeline, no LLM calls, no tool routing.

### 8. Update `moa exec` in `moa-cli/src/exec.rs`
The CLI exec mode should also consume `RuntimeEvent`s from the orchestrator rather than running its own turn loop. Stream events to stderr, collect the final response for stdout.

## Deliverables
```
moa-core/src/traits.rs              # + observe_runtime() on BrainOrchestrator
moa-orchestrator/src/local.rs       # Unified turn engine, trait impl
moa-orchestrator/src/cloud_runtime.rs # Stub observe_runtime()
moa-brain/src/harness.rs            # Wrapper around streamed turn
moa-tui/src/runner.rs               # Simplified to stream consumer
moa-cli/src/exec.rs                 # Simplified to stream consumer
```

## Acceptance criteria
1. `BrainOrchestrator::observe_runtime()` exists on the trait and is implemented by `LocalOrchestrator`.
2. There is ONE streaming turn engine, not two parallel implementations.
3. `run_brain_turn_with_tools()` still works as a buffered wrapper (backward compat).
4. TUI renders streaming assistant text, tool cards, and approval prompts — same UX as before.
5. `moa exec` streams progress to stderr, final response to stdout — same behavior as before.
6. `LocalChatRuntime` does NOT contain pipeline compilation, LLM calls, or tool routing logic.
7. All existing tests pass.

## Tests

**Unit tests (moa-brain):**
- `run_brain_turn_with_tools()` still works with the new internal implementation
- Buffered result matches the streamed events (AssistantFinished text == collected deltas)

**Unit tests (moa-orchestrator):**
- `observe_runtime()` returns a receiver that gets `RuntimeEvent`s during a turn
- `RuntimeEvent::AssistantDelta` events are emitted during streaming
- `RuntimeEvent::ToolUpdate` events are emitted for tool calls
- `RuntimeEvent::ApprovalRequested` is emitted for approval-requiring tools
- `RuntimeEvent::TurnCompleted` is the last event of a turn

**Integration tests:**
- Start session → observe_runtime → submit prompt → collect all RuntimeEvents → verify sequence: AssistantStarted → AssistantDelta* → AssistantFinished → TurnCompleted
- Start session with tool use → verify ToolUpdate events appear in the stream
- Resume session from events → verify the stream works for resumed sessions too

**TUI tests:**
- Verify TUI chat view still works end-to-end (this is a behavior-preserving refactor)

```bash
cargo test -p moa-core
cargo test -p moa-brain
cargo test -p moa-orchestrator
cargo test -p moa-tui
cargo test -p moa-cli
```
