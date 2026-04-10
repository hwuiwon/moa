# Step 34: Postgres SessionStore (in moa-session)

## What this step is about
Add a complete PostgreSQL `SessionStore` and `ApprovalRuleStore` implementation inside the existing `moa-session` crate, gated behind a `postgres` feature flag. This replaces the earlier plan of a separate `moa-session-postgres` crate — keeping both backends in one crate is simpler, avoids cross-crate test harness gymnastics, and lets shared helpers stay naturally co-located.

If step 33 created a `moa-session-postgres` crate, remove it. Everything lives in `moa-session`.

## Files to read
- `moa-session/src/turso.rs` — reference implementation (translate logic, not SQL syntax)
- `moa-session/src/schema.rs` — SQLite DDL (translate to Postgres types)
- `moa-session/src/queries.rs` — row-mapping helpers (libsql `Row` specific)
- `moa-session/src/lib.rs` — current exports, factory function from step 33
- `moa-session/Cargo.toml` — current deps
- `moa-core/src/traits.rs` — `SessionStore` trait
- `moa-core/src/config.rs` — `DatabaseBackend` enum from step 33
- `moa-security/src/policies.rs` — `ApprovalRuleStore` trait

## Goal
`moa-session` has two feature-gated backends: `turso` (default, libsql) and `postgres` (sqlx). The factory function from step 33 routes to the correct one based on config. Both pass an identical shared test suite.

## Rules
- `libsql` becomes an **optional** dependency behind the `turso` feature (default-on).
- `sqlx` with postgres features is an **optional** dependency behind the `postgres` feature.
- At least one backend feature must be enabled at compile time. Add a `compile_error!` if neither is enabled.
- Use native Postgres types: `UUID`, `TIMESTAMPTZ`, `JSONB`, `BIGINT`.
- Use Postgres FTS: `tsvector` + `tsquery` + `GIN` index for event search. Not FTS5.
- Use `sqlx::migrate!()` with embedded migrations.
- Handle Neon cold-start: 10s connect timeout + retry on initial connection.
- If `moa-session-postgres/` exists from step 33, delete it and remove it from the workspace `Cargo.toml` members list.

## Tasks

### 1. Restructure `moa-session/Cargo.toml`

```toml
[package]
name = "moa-session"
version.workspace = true
edition.workspace = true

[features]
default = ["turso"]
turso = ["dep:libsql"]
postgres = ["dep:sqlx"]

[dependencies]
async-trait.workspace = true
chrono.workspace = true
moa-core = { path = "../moa-core" }
moa-security = { path = "../moa-security" }
serde_json.workspace = true
tokio.workspace = true
tracing.workspace = true
uuid.workspace = true

# Backend-specific (optional)
libsql = { version = "0.9.30", optional = true, default-features = false, features = ["core", "remote", "replication", "sync", "tls"] }
sqlx = { version = "0.8", optional = true, features = ["runtime-tokio", "tls-rustls", "postgres", "migrate", "json", "uuid", "chrono"] }

[dev-dependencies]
tempfile = "3"
```

### 2. Gate existing Turso code behind `#[cfg(feature = "turso")]`

In `moa-session/src/lib.rs`:
```rust
#[cfg(feature = "turso")]
pub mod turso;
#[cfg(feature = "turso")]
pub mod schema_turso;  // rename current schema.rs

#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod schema_postgres;

pub mod queries;  // shared helpers that work for both (or split if needed)

#[cfg(not(any(feature = "turso", feature = "postgres")))]
compile_error!("At least one session store backend must be enabled: 'turso' or 'postgres'");
```

Rename `schema.rs` → `schema_turso.rs`. Gate `turso.rs` imports and the `TursoSessionStore` export behind `#[cfg(feature = "turso")]`.

### 3. Update the factory function
The factory from step 33 should now reference the in-crate modules:

```rust
pub async fn create_session_store(config: &MoaConfig) -> Result<Arc<dyn SessionStore + 'static>> {
    match config.database_backend() {
        #[cfg(feature = "turso")]
        DatabaseBackend::Turso => {
            let store = turso::TursoSessionStore::from_config(config).await?;
            Ok(Arc::new(store))
        }
        #[cfg(not(feature = "turso"))]
        DatabaseBackend::Turso => {
            Err(MoaError::ConfigError("Turso backend requires the 'turso' feature".into()))
        }
        #[cfg(feature = "postgres")]
        DatabaseBackend::Postgres => {
            let store = postgres::PostgresSessionStore::from_config(config).await?;
            Ok(Arc::new(store))
        }
        #[cfg(not(feature = "postgres"))]
        DatabaseBackend::Postgres => {
            Err(MoaError::ConfigError("Postgres backend requires the 'postgres' feature".into()))
        }
    }
}
```

### 4. Create Postgres migrations directory
Create `moa-session/migrations/postgres/001_initial.sql`:

```sql
CREATE TABLE IF NOT EXISTS sessions (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT,
    status TEXT NOT NULL DEFAULT 'created',
    platform TEXT,
    platform_channel TEXT,
    model TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    parent_session_id UUID REFERENCES sessions(id),
    total_input_tokens BIGINT DEFAULT 0,
    total_output_tokens BIGINT DEFAULT 0,
    total_cost_cents BIGINT DEFAULT 0,
    event_count BIGINT DEFAULT 0,
    last_checkpoint_seq BIGINT
);

CREATE INDEX IF NOT EXISTS idx_sessions_workspace ON sessions(workspace_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_status ON sessions(status);

CREATE TABLE IF NOT EXISTS events (
    id UUID PRIMARY KEY,
    session_id UUID NOT NULL REFERENCES sessions(id),
    sequence_num BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    brain_id TEXT,
    hand_id TEXT,
    token_count INTEGER,
    search_vector TSVECTOR GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(event_type, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(payload::text, '')), 'B')
    ) STORED,
    UNIQUE(session_id, sequence_num)
);

CREATE INDEX IF NOT EXISTS idx_events_session_seq ON events(session_id, sequence_num);
CREATE INDEX IF NOT EXISTS idx_events_session_type ON events(session_id, event_type);
CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
CREATE INDEX IF NOT EXISTS idx_events_fts ON events USING GIN(search_vector);

CREATE TABLE IF NOT EXISTS approval_rules (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    action TEXT NOT NULL,
    scope TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE(workspace_id, tool, pattern)
);

CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_active TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    session_count BIGINT DEFAULT 0
);

CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    display_name TEXT,
    platform_links JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_active TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

### 5. Create `moa-session/src/postgres.rs`

**Struct + connection:**
```rust
pub struct PostgresSessionStore {
    pool: PgPool,
}

impl PostgresSessionStore {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = Self::connect_with_retry(database_url, 3).await?;
        sqlx::migrate!("migrations/postgres")
            .run(&pool)
            .await
            .map_err(|e| MoaError::StorageError(format!("migration failed: {e}")))?;
        Ok(Self { pool })
    }

    pub async fn from_config(config: &MoaConfig) -> Result<Self> {
        let url = config.database_url()?;
        Self::new(&url).await
    }

    async fn connect_with_retry(url: &str, max_retries: u32) -> Result<PgPool> {
        for attempt in 1..=max_retries {
            match PgPoolOptions::new()
                .min_connections(1)
                .max_connections(5)
                .acquire_timeout(Duration::from_secs(10))
                .connect(url)
                .await
            {
                Ok(pool) => return Ok(pool),
                Err(e) if attempt < max_retries => {
                    tracing::warn!(attempt, error = %e, "postgres connection failed, retrying");
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
                Err(e) => return Err(MoaError::StorageError(
                    format!("postgres connection failed after {max_retries} attempts: {e}")
                )),
            }
        }
        unreachable!()
    }
}
```

**Implement `SessionStore`** — translate every method from `turso.rs`. Key syntax differences:

| SQLite/libsql | Postgres/sqlx |
|---|---|
| `?` params | `$1, $2, ...` params |
| `TEXT` for UUIDs | native `UUID` |
| `TEXT` for timestamps | `TIMESTAMPTZ` |
| `INTEGER` for booleans | native `BOOL` (if used) |
| `TEXT` for JSON payload | `JSONB` — pass `serde_json::Value` directly |
| `Row::get_value(index)` | `Row::get::<Type, _>(index)` or `FromRow` derive |
| FTS5 `MATCH` | `search_vector @@ plainto_tsquery('english', $1)` |
| `rank` from FTS5 | `ts_rank(search_vector, plainto_tsquery(...))` |

**Implement `ApprovalRuleStore`** — same CRUD, Postgres syntax. Use `ON CONFLICT (workspace_id, tool, pattern) DO UPDATE` for upserts.

### 6. Split or gate `queries.rs`
The current `queries.rs` uses `libsql::Row` and `libsql::Value`. Two options:
- **Option A (recommended)**: Gate `queries.rs` behind `#[cfg(feature = "turso")]` and create `queries_postgres.rs` gated behind `#[cfg(feature = "postgres")]` using `sqlx::Row` or `sqlx::FromRow`.
- **Option B**: Rename `queries.rs` → `queries_turso.rs`, create `queries_postgres.rs`.

The Postgres query helpers should use `sqlx::FromRow` derive macro where practical, reducing manual row mapping.

### 7. Write shared test harness
Create `moa-session/tests/shared/mod.rs` with backend-agnostic test functions that take `&(impl SessionStore + ApprovalRuleStore)`:

```rust
pub async fn test_session_lifecycle(store: &(impl SessionStore + ApprovalRuleStore)) {
    test_create_and_get_session(store).await;
    test_emit_and_get_events(store).await;
    test_event_range_filters(store).await;
    test_event_search(store).await;
    test_session_status_update(store).await;
    test_list_sessions(store).await;
    test_approval_rules_crud(store).await;
}
```

**Turso tests** (`session_store.rs`) call shared harness with a temp `TursoSessionStore`.
**Postgres tests** (`postgres_store.rs`) call shared harness with `PostgresSessionStore`, gated behind `#[ignore]` and `TEST_DATABASE_URL`.

### 8. Clean up separate crate if it exists
If `moa-session-postgres/` exists:
- `rm -rf moa-session-postgres/`
- Remove from workspace `Cargo.toml` members list
- Remove any dep references in other crates

### 9. Update workspace feature flags
In root `Cargo.toml`:
```toml
[features]
postgres = ["moa-session/postgres"]
```

## Deliverables
```
moa-session/
├── Cargo.toml                         # turso + postgres features
├── migrations/
│   └── postgres/
│       └── 001_initial.sql
├── src/
│   ├── lib.rs                         # feature-gated modules, factory
│   ├── turso.rs                       # #[cfg(feature = "turso")]
│   ├── postgres.rs                    # #[cfg(feature = "postgres")]
│   ├── schema_turso.rs                # renamed from schema.rs
│   ├── schema_postgres.rs             # migration runner
│   ├── queries.rs (or queries_turso.rs) # libsql row helpers
│   └── queries_postgres.rs            # sqlx row helpers
└── tests/
    ├── shared/mod.rs                  # trait-level test functions
    ├── session_store.rs               # turso → shared
    └── postgres_store.rs              # postgres → shared (#[ignore])
Cargo.toml                            # workspace postgres feature
```

## Acceptance criteria
1. `cargo build -p moa-session` compiles (default = turso only, no sqlx pulled in).
2. `cargo build -p moa-session --features postgres` compiles with sqlx.
3. `cargo build -p moa-session --features postgres --no-default-features` compiles (postgres only, no libsql).
4. `cargo build -p moa-session --no-default-features` fails with `compile_error!`.
5. All existing Turso tests pass with `cargo test -p moa-session`.
6. All shared tests pass against Postgres: `TEST_DATABASE_URL=... cargo test -p moa-session --features postgres -- --ignored`.
7. Event search uses `tsvector @@ plainto_tsquery` on Postgres, FTS5 `MATCH` on Turso.
8. Event payloads stored as `JSONB` on Postgres.
9. UUIDs stored as native `UUID` on Postgres.
10. Neon cold-start retries work (connect timeout 10s, 3 attempts).
11. No `moa-session-postgres` crate exists in the workspace.
12. The factory function routes correctly for both backends.

## Tests

**Shared harness (runs against both backends):**
- Create session → get → all fields match
- Emit 10 events → get with range → correct count and ordering
- Emit events → search with keyword → FTS returns matches
- Update status → get → status changed
- List sessions with workspace filter → correct filtering
- Approval rules: create → list → match → update → delete

**Postgres-specific (in `postgres_store.rs`, `#[ignore]`):**
- JSONB round-trip: store event with nested JSON → retrieve → deep-equal
- UUID native: verify session ID is Postgres UUID
- Concurrent `emit_event`: 10 parallel calls → all succeed, unique sequence nums
- Connection retry: verify retry logic fires on initial failure

```bash
# Default (turso)
cargo test -p moa-session

# Both features compiled
cargo test -p moa-session --features postgres

# Postgres integration (requires database)
TEST_DATABASE_URL="postgres://localhost/moa_test" \
  cargo test -p moa-session --features postgres -- --ignored

# Neon
TEST_DATABASE_URL="postgres://user:pass@ep-xxx.us-east-2.aws.neon.tech/moa_test?sslmode=require" \
  cargo test -p moa-session --features postgres -- --ignored
```
