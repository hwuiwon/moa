# Step 33: Extract Database Abstraction Layer

## What this step is about
`moa-session` is currently hardwired to `libsql` (Turso). The `SessionStore` trait is clean, but the crate itself — schema DDL, query helpers, row mapping, FTS setup — is all libSQL-specific. Before adding Postgres, we need a clean separation: shared types and helpers that both backends use, isolated from the driver-specific code.

This step restructures `moa-session` into a multi-backend architecture and creates a `moa-session-postgres` crate alongside the existing Turso implementation.

## Files to read
- `moa-session/src/lib.rs` — current exports
- `moa-session/src/turso.rs` — `TursoSessionStore` (742 lines)
- `moa-session/src/schema.rs` — SQLite DDL strings
- `moa-session/src/queries.rs` — row-mapping helpers (libsql `Row` specific)
- `moa-core/src/traits.rs` — `SessionStore` trait
- `moa-security/src/policies.rs` — `ApprovalRuleStore` trait (also implemented by TursoSessionStore)
- `moa-session/tests/session_store.rs` — existing tests
- `moa-memory/src/fts.rs` — FTS5 index (also needs Postgres later, but NOT in this step)

## Goal
The workspace has two session store crates behind the same `SessionStore` trait: `moa-session` (renamed to `moa-session-turso` or kept as `moa-session` with a `turso` feature) and `moa-session-postgres`. The orchestrator and CLI select the backend at runtime based on `DATABASE_URL` or config.

## Rules
- The `SessionStore` and `ApprovalRuleStore` traits in `moa-core` do NOT change.
- Choose ONE of these crate structures (recommend option B for simplicity):
  - **Option A**: Split into `moa-session-core` (shared test harness, helpers) + `moa-session-turso` + `moa-session-postgres`
  - **Option B**: Keep `moa-session` as the turso impl with a feature-gated `postgres` module, and add `moa-session-postgres` as a separate crate
  - **Option C**: Keep `moa-session` as-is (turso), add `moa-session-postgres` as a peer crate. Share nothing except the traits from `moa-core`. (Simplest, some query logic duplication)
- Whichever structure you choose, the test suite should be expressible as a shared set of trait-level tests that run against both backends.
- Do NOT implement the Postgres backend in this step — just prepare the architecture so step 34 can slot it in cleanly.

## Tasks

### 1. Create a shared test harness for `SessionStore`
Extract the existing `session_store.rs` tests into a reusable test module that works against any `SessionStore` implementation:

```rust
// moa-session/tests/shared/mod.rs (or a dedicated test-support crate)
pub async fn test_create_and_get_session(store: &dyn SessionStore) { ... }
pub async fn test_emit_and_get_events(store: &dyn SessionStore) { ... }
pub async fn test_event_search(store: &dyn SessionStore) { ... }
pub async fn test_session_status_update(store: &dyn SessionStore) { ... }
pub async fn test_list_sessions_with_filter(store: &dyn SessionStore) { ... }
pub async fn test_approval_rules(store: &(dyn SessionStore + ApprovalRuleStore)) { ... }
```

The existing Turso tests call these with a `TursoSessionStore` instance. The Postgres tests (step 34) will call them with a `PostgresSessionStore` instance.

### 2. Add a `DatabaseBackend` enum and factory to config
In `moa-core/src/config.rs`, add:
```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackend {
    /// SQLite/Turso (default, zero-setup local)
    Turso,
    /// PostgreSQL (Neon, Supabase, self-hosted, etc.)
    Postgres,
}
```

Add a config field:
```toml
[database]
backend = "turso"                    # turso | postgres
url = "~/.moa/sessions.db"          # SQLite path or postgres:// URL
# Postgres-specific
pool_min = 1
pool_max = 5
connect_timeout_secs = 10
```

### 3. Add a session store factory function
In `moa-session/src/lib.rs` (or a new `moa-session/src/factory.rs`):
```rust
pub async fn create_session_store(config: &MoaConfig) -> Result<Arc<dyn SessionStore>> {
    match config.database.backend {
        DatabaseBackend::Turso => {
            let store = TursoSessionStore::from_config(config).await?;
            Ok(Arc::new(store))
        }
        DatabaseBackend::Postgres => {
            #[cfg(feature = "postgres")]
            {
                let store = moa_session_postgres::PostgresSessionStore::from_config(config).await?;
                Ok(Arc::new(store))
            }
            #[cfg(not(feature = "postgres"))]
            {
                Err(MoaError::ConfigError(
                    "Postgres backend requires the 'postgres' feature flag".into()
                ))
            }
        }
    }
}
```

### 4. Update CLI/orchestrator to use the factory
Replace direct `TursoSessionStore::from_config()` calls in:
- `moa-cli/src/main.rs` or `moa-cli/src/exec.rs`
- `moa-orchestrator/src/local.rs` (if it constructs the store)
- Any other place that directly constructs a `TursoSessionStore`

Use `create_session_store(config)` instead.

### 5. Add `postgres` feature flag to workspace
In workspace `Cargo.toml`:
```toml
[features]
postgres = ["moa-session/postgres"]
```

In `moa-session/Cargo.toml`:
```toml
[features]
default = []
postgres = ["dep:moa-session-postgres"]

[dependencies]
moa-session-postgres = { path = "../moa-session-postgres", optional = true }
```

### 6. Create the `moa-session-postgres` crate scaffold
```
moa-session-postgres/
├── Cargo.toml
├── src/
│   ├── lib.rs        # PostgresSessionStore struct (stub, panics with "not yet implemented")
│   ├── schema.rs     # (empty, for step 34)
│   └── queries.rs    # (empty, for step 34)
```

`Cargo.toml`:
```toml
[package]
name = "moa-session-postgres"
version.workspace = true
edition.workspace = true

[dependencies]
async-trait.workspace = true
chrono.workspace = true
moa-core = { path = "../moa-core" }
moa-security = { path = "../moa-security" }
serde_json.workspace = true
sqlx = { version = "0.8", features = ["runtime-tokio", "tls-rustls", "postgres", "migrate", "json", "uuid", "chrono"] }
tokio.workspace = true
tracing.workspace = true
uuid.workspace = true
```

The stub `PostgresSessionStore` should implement `SessionStore` and `ApprovalRuleStore` with all methods returning `unimplemented!()`. This lets the crate compile and the feature flag work before step 34 fills in the implementation.

## Deliverables
```
moa-core/src/config.rs                    # DatabaseBackend enum, database config section
moa-session/src/lib.rs                    # Factory function, feature-gated postgres
moa-session/src/turso.rs                  # (no change, just verifying it still works)
moa-session/tests/shared/mod.rs           # Shared trait-level test harness
moa-session/tests/session_store.rs        # Updated to use shared harness
moa-session/Cargo.toml                    # + postgres feature + optional dep
moa-session-postgres/Cargo.toml           # New crate
moa-session-postgres/src/lib.rs           # Stub PostgresSessionStore
moa-session-postgres/src/schema.rs        # Empty scaffold
moa-session-postgres/src/queries.rs       # Empty scaffold
Cargo.toml                                # + moa-session-postgres member + postgres feature
moa-cli/src/...                           # Use factory instead of direct TursoSessionStore
moa-orchestrator/src/local.rs             # Use factory instead of direct TursoSessionStore
```

## Acceptance criteria
1. `cargo build` compiles without the `postgres` feature (default = turso only).
2. `cargo build --features postgres` compiles with the stub Postgres crate.
3. All existing Turso tests pass unchanged.
4. The shared test harness exists and the Turso tests call through it.
5. `create_session_store(config)` returns a `TursoSessionStore` by default.
6. `create_session_store(config)` returns `PostgresSessionStore` (stub) when `backend = "postgres"` and the feature is enabled.
7. No direct `TursoSessionStore::from_config()` calls remain outside `moa-session`.

## Tests
```bash
# Default (turso only)
cargo test -p moa-session

# With postgres feature (stub compiles, turso tests still pass)
cargo test -p moa-session --features postgres

# Full workspace
cargo test --workspace
```
