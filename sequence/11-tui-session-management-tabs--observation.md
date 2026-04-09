# Step 11: TUI Session Management (Tabs + Observation)

## What this step is about
Tab bar for multiple sessions, session picker overlay, real-time observation of running sessions, and queue/stop controls.

## Files to read
- `docs/03-communication-layer.md` — Tab bar, session picker, observation detail levels, keyboard shortcuts for session management

## Goal
Users see a tab bar at the top with active sessions. They can switch between them, create new sessions, and observe running sessions with live tool call updates. Queue and Stop buttons work.

## Tasks
1. **`moa-tui/src/widgets/toolbar.rs`**: Tab bar widget showing session tabs with status icons (🔄 running, ⏸ waiting, ✅ done, ❌ error). Max 8 visible. `Alt+1-9` direct switch, `Alt+[/]` cycle, `Ctrl+N` new.
2. **`moa-tui/src/views/sessions.rs`**: Session picker overlay (fuzzy search via `nucleo` crate). Shows all sessions with workspace, status, last message. `Ctrl+O, S` to open.
3. **Update `moa-tui/src/app.rs`**: Multi-session state. Active session ID. Background sessions continue via orchestrator.
4. **Observation rendering**: When viewing a session that's running, show live streaming events. Throttle updates to ~30 FPS.
5. **Queue/Stop controls**: In the footer, show `[Ctrl+Q: queue message]` and `[Ctrl+X, S: stop]` when a session is running.

## Deliverables
`moa-tui/src/widgets/toolbar.rs`, `moa-tui/src/views/sessions.rs`, updated `app.rs`

## Acceptance criteria
1. Tab bar shows active sessions with correct status icons
2. Switching tabs switches the chat view to that session
3. Session picker opens with fuzzy search
4. New session creates via `Ctrl+N`
5. Observation shows live events as they happen
6. Stop sends cancel signal and session stops

## Tests
- Unit test: Tab bar renders correct number of tabs with correct icons
- Unit test: Session picker fuzzy search matches correctly
- Manual test: Open 3 sessions, switch between them, verify independent state

---

