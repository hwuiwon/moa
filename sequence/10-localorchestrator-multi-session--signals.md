# Step 10: LocalOrchestrator (Multi-Session + Signals)

## What this step is about
The `LocalOrchestrator` that manages multiple concurrent brain sessions using tokio tasks and mpsc channels.

## Files to read
- `docs/02-brain-orchestration.md` — `LocalOrchestrator` implementation, signal handling, brain loop

## Goal
Multiple sessions can run concurrently. Users can send signals (queue messages, stop, approve) to any running session. Sessions survive across TUI views (switching tabs doesn't kill the brain).

## Tasks
1. **`moa-orchestrator/src/local.rs`**: `LocalOrchestrator` implementing `BrainOrchestrator` trait. Manages `HashMap<SessionId, LocalBrainHandle>` with tokio JoinHandles and mpsc signal channels.
2. **`start_session()`**: Spawn a new tokio task running the brain loop from Step 04.
3. **`resume_session()`**: Wake from session store and spawn brain loop from last event.
4. **`signal()`**: Send `SessionSignal` via the session's mpsc channel.
5. **`observe()`**: Return a `broadcast::Receiver` for session events.
6. **`list_sessions()`**: Return status of all active sessions.
7. **`schedule_cron()`**: Use `tokio-cron-scheduler` for periodic jobs.
8. **Replace the simplified single-session runner in `moa-tui`** with the `LocalOrchestrator`.

## Deliverables
`moa-orchestrator/src/local.rs`, `moa-orchestrator/src/lib.rs`, updated TUI wiring.

## Acceptance criteria
1. Can start 3 sessions concurrently
2. Sending QueueMessage to a running session delivers it after current turn
3. SoftCancel completes current tool call then stops
4. HardCancel aborts immediately
5. Observe returns a stream of events
6. Session persists if TUI disconnects (task keeps running)
7. `resume_session` correctly wakes from session store

## Tests
- Integration test: Start two sessions, send messages to both, verify both produce responses
- Integration test: Signal SoftCancel → session status becomes Cancelled
- Integration test: Queue a message during active run → message processed after turn completes
- Integration test: Observe stream receives events in order

```bash
cargo test -p moa-orchestrator
```

---

