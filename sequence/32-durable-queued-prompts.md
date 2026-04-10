# Step 32: Durable Queued Prompts

## What this step is about
When a user sends a message while a session is actively running, the prompt is buffered in memory and flushed as a `QueuedMessage` event only after the current turn completes. This fixes an Anthropic API ordering bug (user message persisted before the in-flight assistant reply), but introduces a durability gap: if the process crashes between queue and flush, the user's message is lost. This violates MOA's core principle that the session log is the crash-recovery mechanism.

This step adds a durable pending-signal store so queued prompts survive crashes without reintroducing the message-ordering problem.

## Files to read
- `moa-orchestrator/src/local.rs` — `buffer_queued_message()`, `flush_queued_messages()`, `flush_next_queued_message()`, the in-memory `Vec<UserMessage>` buffer
- `moa-core/src/events.rs` — `Event::QueuedMessage`, `Event::UserMessage`
- `moa-core/src/types.rs` — `SessionSignal::QueueMessage`, `UserMessage`
- `moa-session/src/turso.rs` — session store implementation, schema
- `moa-core/src/traits.rs` — `SessionStore` trait

## Goal
Queued prompts are persisted immediately when received, but are NOT included in the LLM conversation context until flushed. Crash recovery can find and re-queue pending prompts. The Anthropic message-ordering invariant (conversation must end with a user message, not assistant) is preserved.

## Rules
- The solution must not reintroduce the ordering bug. Pending prompts must be distinguishable from "ready" messages in the event log so the context pipeline does NOT include them prematurely.
- Keep it simple. The lightest correct solution is a separate `pending_signals` table (not events), since pending signals are transient state that gets resolved, not append-only history.
- Alternatively, use a new event type `Event::PendingQueuedMessage` that the history compiler explicitly excludes, which gets resolved to `Event::QueuedMessage` at flush time. This keeps everything in the existing event log without a new table.
- Either approach is acceptable. Choose whichever integrates more cleanly with the existing flush logic.
- On crash recovery (`wake()` / `resume_session()`), detect unresolved pending prompts and re-buffer them.

## Tasks

### Option A: Pending signals table (recommended)

#### 1. Add `pending_signals` table to the session schema
```sql
CREATE TABLE pending_signals (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    signal_type TEXT NOT NULL,         -- 'queue_message'
    payload TEXT NOT NULL,             -- JSON serialized UserMessage
    created_at TEXT NOT NULL,
    resolved_at TEXT,                  -- NULL until flushed
    FOREIGN KEY (session_id) REFERENCES sessions(id)
);
CREATE INDEX idx_pending_session ON pending_signals(session_id, resolved_at);
```

#### 2. Add methods to `SessionStore` trait
```rust
async fn store_pending_signal(
    &self,
    session_id: SessionId,
    signal: PendingSignal,
) -> Result<PendingSignalId>;

async fn get_pending_signals(
    &self,
    session_id: SessionId,
) -> Result<Vec<PendingSignal>>;

async fn resolve_pending_signal(
    &self,
    signal_id: PendingSignalId,
) -> Result<()>;
```

#### 3. Define `PendingSignal` type in `moa-core/src/types.rs`
```rust
pub struct PendingSignal {
    pub id: PendingSignalId,
    pub session_id: SessionId,
    pub signal_type: PendingSignalType,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

pub enum PendingSignalType {
    QueueMessage,
}
```

#### 4. Update the orchestrator's queue handling
In `moa-orchestrator/src/local.rs`:

**On receive** (when `SessionSignal::QueueMessage` arrives during an active turn):
```rust
// Instead of: queued_messages.push(message);
// Do:
let signal_id = session_store.store_pending_signal(
    session_id,
    PendingSignal::queue_message(message),
).await?;
pending_ids.push(signal_id);
```

**On flush** (at turn boundary):
```rust
// Load pending signals
let pending = session_store.get_pending_signals(session_id).await?;
for signal in pending {
    // Emit the real QueuedMessage event
    session_store.emit_event(session_id, Event::QueuedMessage { ... }).await?;
    // Mark as resolved
    session_store.resolve_pending_signal(signal.id).await?;
}
```

#### 5. Update crash recovery
In `resume_session()` or `wake()`:
```rust
let pending = session_store.get_pending_signals(session_id).await?;
for signal in pending {
    // Re-buffer into the in-memory queue
    queued_messages.push(signal.into_user_message());
}
```

#### 6. Implement in `TursoSessionStore`
Add the `pending_signals` table to the migration/schema. Implement the three new trait methods.

### Option B: Pending event type (alternative)
If modifying the `SessionStore` trait feels too heavy for this:

1. Add `Event::PendingQueuedMessage { text, queued_at }` to the event enum
2. Emit it immediately when a message is queued
3. At flush time, emit `Event::QueuedMessage` (the real one)
4. The history compiler (stage 6) explicitly EXCLUDES `PendingQueuedMessage` events from context
5. On crash recovery, scan for `PendingQueuedMessage` events that have no corresponding `QueuedMessage` after them → those are unresolved

**Downside**: the event log now contains "resolved" pending events that are noise. Option A is cleaner.

## Deliverables
```
moa-core/src/types.rs              # PendingSignal, PendingSignalType, PendingSignalId
moa-core/src/traits.rs             # + store/get/resolve pending signal methods on SessionStore
moa-session/src/turso.rs           # pending_signals table, trait impl
moa-orchestrator/src/local.rs      # Persist on queue, resolve on flush, recover on resume
```

## Acceptance criteria
1. A prompt queued during an active turn is immediately persisted (not just in memory).
2. If the process crashes after queuing but before flush, the prompt is recovered on resume.
3. The context pipeline does NOT include pending (unflushed) prompts in the LLM conversation.
4. After flush, the `QueuedMessage` event appears in the log at the correct position (after the assistant reply).
5. The Anthropic message-ordering invariant is preserved — no regression.
6. Resolved pending signals are cleaned up (resolved_at is set).
7. All existing tests pass.

## Tests

**Unit tests (moa-session):**
- `store_pending_signal` → signal persists in the table
- `get_pending_signals` → returns only unresolved signals for the session
- `resolve_pending_signal` → signal no longer appears in `get_pending_signals`
- Round-trip: store → get → verify payload matches

**Unit tests (moa-orchestrator):**
- Queue a message during active turn → pending signal is stored
- Turn completes → pending signal is resolved, `QueuedMessage` event emitted
- Simulate crash: store pending signal, don't resolve → `resume_session` recovers it
- Two queued messages → both are flushed in order after turn completes

**Integration test:**
- Start session → submit prompt → while running, queue a follow-up → verify:
  1. Pending signal exists in DB immediately
  2. After turn completes, `QueuedMessage` event is in the log
  3. Pending signal is resolved
  4. The follow-up is processed as the next turn
- Start session → submit prompt → queue follow-up → kill orchestrator → resume → follow-up is recovered and processed

```bash
cargo test -p moa-core
cargo test -p moa-session
cargo test -p moa-orchestrator
```
