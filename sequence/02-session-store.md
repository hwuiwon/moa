# Step 02: Session Store (Turso/libSQL)

## What this step is about

Implementing the `SessionStore` trait using Turso/libSQL. In local mode this is a SQLite file at `~/.moa/sessions.db`. The same code works with a Turso Cloud URL for cloud mode.

## Files to read

- `docs/05-session-event-log.md` — Full SQL schema, event types, core operations (emit_event, get_events, wake)
- `docs/01-architecture-overview.md` — `SessionStore` trait definition

## Goal

A working session store that can create sessions, emit events, query events by range, update session status, and recover a brain's state via `wake()`. All backed by SQLite locally with no external dependencies.

## Rules

- Use the `libsql` crate for database access (Turso's Rust client — works with both local SQLite and Turso Cloud)
- If `libsql` proves problematic, fall back to `rusqlite` for local and keep Turso for cloud behind the trait
- Schema must match `docs/05-session-event-log.md` exactly
- Migrations run automatically on startup (embedded in the binary)
- `sequence_num` is monotonically increasing per session, enforced by UNIQUE constraint
- All operations must be async
- FTS index on events (`events_fts`) for cross-session search

## Tasks

1. **Add dependencies** to `moa-session/Cargo.toml`: `libsql` (or `rusqlite` + `tokio`), `moa-core`
2. **Implement schema** in `moa-session/src/schema.rs`: SQL CREATE statements as const strings, a `migrate()` function that runs them idempotently
3. **Implement `TursoSessionStore`** in `moa-session/src/turso.rs`:
   - `new(url: &str)` — connect to local file or Turso Cloud URL
   - `new_local(path: &Path)` — convenience for local SQLite
   - All `SessionStore` trait methods
4. **Implement `emit_event()`**: Insert event, update session metadata (event_count, token totals, cost), update FTS index
5. **Implement `get_events()`**: Query with range filters (from_seq, to_seq, event_types, limit)
6. **Implement `wake()`**: Find last checkpoint, load events since checkpoint, return `WakeContext`
7. **Implement `search_events()`**: FTS5 MATCH query across sessions
8. **Implement `list_sessions()`**: Filtered by workspace, user, status, with pagination

## How to implement

Start with the schema. Create tables: `sessions`, `events`, `events_fts`, `approval_rules`, `workspaces`, `users`. Create indexes. Run migrations in a `migrate()` function that checks if tables exist before creating them.

For `emit_event`, use a transaction: insert event → update session counters → insert into FTS. Generate `sequence_num` by selecting `MAX(sequence_num) + 1` for the session (within the transaction to avoid races).

For `wake`, find the last `Checkpoint` event for the session, load all events after it. Return a `WakeContext` struct containing the session metadata, optional checkpoint summary, and recent events.

The store should be constructable with just a file path for local use:
```rust
let store = TursoSessionStore::new_local(Path::new("~/.moa/sessions.db")).await?;
```

## Deliverables

```
moa-session/
├── Cargo.toml
└── src/
    ├── lib.rs          # pub mod, re-exports TursoSessionStore
    ├── turso.rs        # TursoSessionStore implementing SessionStore trait
    ├── schema.rs       # SQL DDL + migrate()
    └── queries.rs      # Helper query functions
```

## Acceptance criteria

1. `cargo build -p moa-session` succeeds
2. `cargo test -p moa-session` passes all tests
3. Can create a session, emit 100 events, query them by range
4. `wake()` correctly finds the last checkpoint and returns events after it
5. FTS search finds events by content
6. Session metadata (token counts, cost, event_count) updates correctly on each emit
7. Schema creates cleanly on first run, is idempotent on subsequent runs
8. Works with a local SQLite file (no network)

## Tests

### Integration tests in `moa-session/tests/session_store.rs`

```rust
use moa_core::{Event, SessionStatus, EventRange};
use moa_session::TursoSessionStore;
use tempfile::tempdir;

#[tokio::test]
async fn create_session_and_emit_events() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let store = TursoSessionStore::new_local(&db_path).await.unwrap();

    let session_id = store.create_session(SessionMeta {
        workspace_id: "ws1".into(),
        user_id: "u1".into(),
        model: "test-model".into(),
        ..Default::default()
    }).await.unwrap();

    // Emit events
    let seq1 = store.emit_event(session_id.clone(), Event::UserMessage {
        text: "Hello".into(),
        attachments: vec![],
    }).await.unwrap();
    assert_eq!(seq1, 0);

    let seq2 = store.emit_event(session_id.clone(), Event::BrainResponse {
        text: "Hi there".into(),
        model: "test".into(),
        input_tokens: 10,
        output_tokens: 5,
        cost_cents: 1,
        duration_ms: 100,
    }).await.unwrap();
    assert_eq!(seq2, 1);

    // Query all events
    let events = store.get_events(session_id.clone(), EventRange::all()).await.unwrap();
    assert_eq!(events.len(), 2);

    // Check session metadata was updated
    let session = store.get_session(session_id).await.unwrap();
    assert_eq!(session.event_count, 2);
    assert_eq!(session.total_input_tokens, 10);
    assert_eq!(session.total_cost_cents, 1);
}

#[tokio::test]
async fn get_events_with_range_filter() {
    // ... create session, emit 10 events ...
    // Query events 3-7
    let events = store.get_events(session_id, EventRange {
        from_seq: Some(3),
        to_seq: Some(7),
        event_types: None,
        limit: None,
    }).await.unwrap();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0].sequence_num, 3);
    assert_eq!(events[4].sequence_num, 7);
}

#[tokio::test]
async fn get_events_filtered_by_type() {
    // ... create session, emit mix of UserMessage and BrainResponse events ...
    let events = store.get_events(session_id, EventRange {
        event_types: Some(vec!["UserMessage".to_string()]),
        ..Default::default()
    }).await.unwrap();
    // Should only return UserMessage events
    for e in &events {
        assert_eq!(e.event_type, "UserMessage");
    }
}

#[tokio::test]
async fn wake_finds_checkpoint_and_recent_events() {
    // ... create session, emit 5 events, emit Checkpoint, emit 3 more events ...
    let wake_ctx = store.wake(session_id).await.unwrap();
    assert!(wake_ctx.checkpoint_summary.is_some());
    assert_eq!(wake_ctx.recent_events.len(), 3); // only events after checkpoint
}

#[tokio::test]
async fn wake_without_checkpoint_returns_all_events() {
    // ... create session, emit 5 events (no checkpoint) ...
    let wake_ctx = store.wake(session_id).await.unwrap();
    assert!(wake_ctx.checkpoint_summary.is_none());
    assert_eq!(wake_ctx.recent_events.len(), 5);
}

#[tokio::test]
async fn fts_search_finds_events() {
    // ... create session, emit events with specific text ...
    store.emit_event(session_id, Event::UserMessage {
        text: "Fix the OAuth refresh token bug".into(),
        attachments: vec![],
    }).await.unwrap();

    let results = store.search_events("OAuth refresh", EventFilter::default()).await.unwrap();
    assert!(!results.is_empty());
    assert!(results[0].payload.contains("OAuth"));
}

#[tokio::test]
async fn list_sessions_filters_by_workspace() {
    // Create sessions in different workspaces
    // ... create session in ws1, create session in ws2 ...
    let ws1_sessions = store.list_sessions(SessionFilter {
        workspace_id: Some("ws1".into()),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(ws1_sessions.len(), 1);
}

#[tokio::test]
async fn schema_is_idempotent() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    // Create store twice — should not error
    let _store1 = TursoSessionStore::new_local(&db_path).await.unwrap();
    let _store2 = TursoSessionStore::new_local(&db_path).await.unwrap();
}
```

### Run tests

```bash
cargo test -p moa-session -- --nocapture
```

## Notes

- Use `tempfile::tempdir()` in tests to create isolated databases
- The FTS5 virtual table requires SQLite to be compiled with FTS5 support — `libsql` includes this by default
- For the `events_fts` table, use a content-sync approach (`content=events, content_rowid=rowid`) so FTS stays in sync with the main table
- `sequence_num` starts at 0 for each session and increments by 1
