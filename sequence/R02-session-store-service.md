# R02 — `SessionStore` Service

## Purpose

Ship the first real Restate handler: the `SessionStore` Service, which writes and reads session events from Postgres. This is the smallest genuine handler — no brain loop, no tool execution — and establishes the Service pattern for all subsequent service prompts.

End state: `moa-orchestrator` exposes `SessionStore::append_event`, `get_events`, `get_session`, `update_status`, and `search_events` handlers. Each call writes to or reads from Postgres. Unit tests cover all five handlers; an integration test round-trips events through a local `restate-server`.

## Prerequisites

- R01 complete. `Health` service working, `moa-orchestrator` binary compiles and runs.
- Postgres 16+ accessible locally (Neon dev branch or local Docker Postgres).
- `sqlx-cli` installed: `cargo install sqlx-cli --no-default-features --features native-tls,postgres`.

## Read before starting

- `docs/05-session-event-log.md` — the full Postgres schema. R02 implements the `events` and `sessions` table operations.
- `docs/12-restate-architecture.md` — "Handler signatures" and "What stays in Postgres, not in Restate state"
- `moa-core/src/types.rs` — existing `SessionId`, `Event`, `EventRecord` types
- `moa-session/src/lib.rs` — existing `SessionStore` trait and Turso-backed impl (reference only; being replaced)

## Steps

### 1. Postgres schema migration

Create `moa-orchestrator/migrations/001_sessions.sql` with the schema from `docs/05-session-event-log.md`. Use `sqlx migrate` conventions:

```sql
-- 001_sessions.sql
CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    workspace_id UUID NOT NULL,
    user_id UUID NOT NULL,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT,
    platform_channel TEXT,
    model TEXT,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL,
    completed_at TIMESTAMPTZ,
    parent_session_id UUID,
    total_input_tokens BIGINT DEFAULT 0,
    total_output_tokens BIGINT DEFAULT 0,
    total_cost_cents BIGINT DEFAULT 0,
    event_count BIGINT DEFAULT 0,
    last_checkpoint_seq BIGINT
);

CREATE INDEX idx_sessions_workspace ON sessions(workspace_id, updated_at DESC);
CREATE INDEX idx_sessions_tenant ON sessions(tenant_id, updated_at DESC);
CREATE INDEX idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX idx_sessions_status ON sessions(status) WHERE status IN ('running', 'waiting_approval');

CREATE TABLE IF NOT EXISTS events (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    sequence_num BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    brain_id TEXT,
    hand_id TEXT,
    token_count INTEGER,
    UNIQUE(session_id, sequence_num)
) PARTITION BY RANGE (timestamp);

-- Initial monthly partitions created by pg_partman (see R10).
-- For local dev, create one catch-all partition:
CREATE TABLE IF NOT EXISTS events_default PARTITION OF events DEFAULT;

CREATE INDEX idx_events_session_seq ON events(session_id, sequence_num);
CREATE INDEX idx_events_session_type ON events(session_id, event_type);
CREATE INDEX idx_events_timestamp ON events USING BRIN (timestamp);
```

Row-Level Security (RLS) is deferred to Phase 0 cleanup — add `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` and per-tenant policies in a follow-up migration. For R02, enforce tenant scoping in application code.

### 2. Add deps to `moa-orchestrator`

```toml
[dependencies]
sqlx = { version = "0.8", features = ["runtime-tokio", "tls-rustls", "postgres", "json", "uuid", "chrono"] }
deadpool-postgres = "0.14"  # optional; sqlx pool is fine for R02
uuid = { workspace = true, features = ["v4", "v7", "serde"] }
chrono = { workspace = true, features = ["serde"] }
```

### 3. Define types (or import from `moa-core`)

`moa-orchestrator/src/types.rs` (or extend `moa-core/src/types.rs`):

```rust
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{DateTime, Utc};

pub type SessionId = Uuid;
pub type SequenceNum = i64;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum SessionEvent {
    UserMessage { text: String, attachments: Vec<String> },
    BrainResponse { text: String, model: String, input_tokens: usize, output_tokens: usize, cost_cents: u32, duration_ms: u64 },
    ToolCall { tool_id: Uuid, tool_name: String, input: serde_json::Value },
    ToolResult { tool_id: Uuid, output: String, success: bool, duration_ms: u64 },
    ApprovalRequested { tool_call: serde_json::Value, awakeable_id: String },
    ApprovalDecided { awakeable_id: String, decision: serde_json::Value },
    Checkpoint { summary: String, events_summarized: u64, token_count: usize },
    Error { message: String, recoverable: bool },
    // ... extend as needed, mirror docs/05
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: Uuid,
    pub session_id: SessionId,
    pub sequence_num: SequenceNum,
    pub event: SessionEvent,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub tenant_id: Uuid,
    pub workspace_id: Uuid,
    pub user_id: Uuid,
    pub title: Option<String>,
    pub status: SessionStatus,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Running,
    WaitingApproval,
    Idle,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventRange {
    pub from_seq: Option<SequenceNum>,
    pub to_seq: Option<SequenceNum>,
    pub event_types: Option<Vec<String>>,
    pub limit: Option<u32>,
}
```

### 4. Define the Service trait

`moa-orchestrator/src/services/session_store.rs`:

```rust
use restate_sdk::prelude::*;
use crate::types::*;

#[restate_sdk::service]
pub trait SessionStore {
    async fn create_session(
        ctx: Context<'_>,
        meta: SessionMeta,
    ) -> Result<SessionId, HandlerError>;

    async fn append_event(
        ctx: Context<'_>,
        session_id: SessionId,
        event: SessionEvent,
    ) -> Result<SequenceNum, HandlerError>;

    async fn get_events(
        ctx: Context<'_>,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>, HandlerError>;

    async fn get_session(
        ctx: Context<'_>,
        session_id: SessionId,
    ) -> Result<SessionMeta, HandlerError>;

    async fn update_status(
        ctx: Context<'_>,
        session_id: SessionId,
        status: SessionStatus,
    ) -> Result<(), HandlerError>;

    async fn search_events(
        ctx: Context<'_>,
        query: String,
        tenant_id: uuid::Uuid,
        limit: u32,
    ) -> Result<Vec<EventRecord>, HandlerError>;
}
```

### 5. Implement the Service

```rust
pub struct SessionStoreImpl {
    pub pool: sqlx::PgPool,
}

impl SessionStore for SessionStoreImpl {
    async fn create_session(
        ctx: Context<'_>,
        meta: SessionMeta,
    ) -> Result<SessionId, HandlerError> {
        let pool = get_pool_from_ctx(&ctx);

        // ctx.run wraps DB call so it's journaled and replay-safe.
        ctx.run("insert_session", || async move {
            sqlx::query!(
                "INSERT INTO sessions (id, tenant_id, workspace_id, user_id, title, status, model, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                meta.id, meta.tenant_id, meta.workspace_id, meta.user_id,
                meta.title, meta.status.to_string(), meta.model,
                meta.created_at, meta.updated_at,
            )
            .execute(&pool)
            .await?;
            Ok(meta.id)
        })
        .await
        .map_err(HandlerError::from)
    }

    async fn append_event(
        ctx: Context<'_>,
        session_id: SessionId,
        event: SessionEvent,
    ) -> Result<SequenceNum, HandlerError> {
        let pool = get_pool_from_ctx(&ctx);
        let event_json = serde_json::to_value(&event)?;

        ctx.run("insert_event", || async move {
            // Acquire next sequence in a transaction.
            let mut tx = pool.begin().await?;
            let next_seq: i64 = sqlx::query_scalar!(
                "SELECT COALESCE(MAX(sequence_num), 0) + 1 FROM events WHERE session_id = $1",
                session_id
            )
            .fetch_one(&mut *tx).await?
            .unwrap_or(1);

            sqlx::query!(
                "INSERT INTO events (id, session_id, sequence_num, event_type, payload, timestamp)
                 VALUES ($1, $2, $3, $4, $5, NOW())",
                uuid::Uuid::new_v4(), session_id, next_seq,
                event_type_name(&event), event_json,
            )
            .execute(&mut *tx).await?;

            sqlx::query!(
                "UPDATE sessions SET updated_at = NOW(), event_count = event_count + 1 WHERE id = $1",
                session_id
            )
            .execute(&mut *tx).await?;

            tx.commit().await?;
            Ok(next_seq)
        })
        .await
        .map_err(HandlerError::from)
    }

    // ... implement get_events, get_session, update_status, search_events similarly.
}

fn event_type_name(e: &SessionEvent) -> &'static str {
    match e {
        SessionEvent::UserMessage { .. } => "UserMessage",
        SessionEvent::BrainResponse { .. } => "BrainResponse",
        SessionEvent::ToolCall { .. } => "ToolCall",
        SessionEvent::ToolResult { .. } => "ToolResult",
        SessionEvent::ApprovalRequested { .. } => "ApprovalRequested",
        SessionEvent::ApprovalDecided { .. } => "ApprovalDecided",
        SessionEvent::Checkpoint { .. } => "Checkpoint",
        SessionEvent::Error { .. } => "Error",
    }
}
```

The `get_pool_from_ctx` helper retrieves the shared Postgres pool. Approach: stash the pool in a `OnceLock<PgPool>` at binary startup; helper returns `Arc<PgPool>`. Avoid trait-object state because Restate's Service derive macros assume a `self`-less signature.

### 6. Wire the service into the endpoint

`moa-orchestrator/src/main.rs` — extend:

```rust
async fn main() -> anyhow::Result<()> {
    // ... logging init, config load

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(25)
        .connect(&config.postgres_url)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;

    // Stash pool in OnceLock for handlers to access
    let _ = POOL.set(pool.clone());

    HttpServer::new(
        Endpoint::builder()
            .bind(services::health::HealthImpl.serve())
            .bind(services::session_store::SessionStoreImpl { pool }.serve())
            .build(),
    )
    .listen_and_serve(format!("0.0.0.0:{}", args.port).parse()?)
    .await
}

pub static POOL: std::sync::OnceLock<sqlx::PgPool> = std::sync::OnceLock::new();
```

### 7. Unit tests

`moa-orchestrator/tests/session_store.rs`:

- `append_event_increments_sequence` — write 3 events, assert sequence_num = 1, 2, 3
- `get_events_respects_range` — write 10 events, get range [3..7], assert 5 returned
- `update_status_affects_get_session` — update from Running to Completed, verify via get_session
- `search_events_finds_by_payload` — write events with distinct payloads, search by text

Use `sqlx::test` attribute for auto-rollback per test.

### 8. Integration test

`moa-orchestrator/tests/integration/session_store_e2e.rs`:

- Spin up `restate-test-server` in-process (if available) or run against `docker run restatedev/restate`.
- Register service endpoint.
- Invoke `SessionStore/create_session`, `append_event` x3, `get_events`, assert round-trip matches.

## Files to create or modify

- `moa-orchestrator/migrations/001_sessions.sql` — new
- `moa-orchestrator/src/types.rs` (or extend `moa-core/src/types.rs`) — new types
- `moa-orchestrator/src/services/session_store.rs` — new
- `moa-orchestrator/src/services/mod.rs` — add `pub mod session_store;`
- `moa-orchestrator/src/main.rs` — wire service, add pool
- `moa-orchestrator/Cargo.toml` — add sqlx deps
- `moa-orchestrator/tests/session_store.rs` — unit tests
- `moa-orchestrator/tests/integration/session_store_e2e.rs` — integration test

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` succeeds.
- [ ] `sqlx migrate run --database-url $POSTGRES_URL` applies cleanly.
- [ ] All unit tests pass: `cargo test -p moa-orchestrator session_store`.
- [ ] Integration test passes when `restate-server` is running locally.
- [ ] `restate invocation call 'SessionStore/create_session'` with a valid JSON body returns a UUID.
- [ ] `restate invocation call 'SessionStore/append_event'` after creating a session returns `sequence_num: 1`, subsequent calls return increasing sequence numbers.
- [ ] Events are visible in Postgres: `SELECT * FROM events WHERE session_id = $1 ORDER BY sequence_num`.

## Notes

- **Tenant scoping**: all handlers should check `tenant_id` from the session metadata on reads/writes. For R02, this is enforced in application code; Phase 0 cleanup will add Postgres RLS and the `SET app.tenant_id` invariant.
- **JSONB payload**: stored as `serde_json::Value` in Postgres, deserialized back to `SessionEvent` enum on read. Use `#[serde(tag, content)]` to make the round-trip stable.
- **Partitioning**: R02 uses a default partition; R10 adds `pg_partman` for monthly range partitioning. Do not pre-optimize here.
- **Why `ctx.run()` around DB calls**: Postgres writes are side effects. If the Service invocation is retried (e.g., due to transient network failure), Restate replays the journal and does *not* re-execute the DB write — the result is returned from the journal. This is the only way to get correctness under retry.
- **`sequence_num` acquisition in a transaction** is the simplest correct approach. For higher throughput, switch to a sequence per session_id or an atomic `INSERT ... RETURNING` pattern in a follow-up.

## What R03 expects

- `SessionStore` Service callable.
- DB pool accessible to all handlers via `POOL.get()`.
- Service scaffolding pattern established in `services/` module.
- `SessionEvent` enum defined and extensible.
