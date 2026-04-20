# Step 83 — Postgres Everywhere: Delete All SQLite, Docker Required

_Postgres is the only supported storage backend for MOA. SQLite is removed from both `moa-session` (session event log) and `moa-memory` (wiki search index). Docker is assumed to be available for local development. The repo ships a `docker-compose.yml` that users `up` once. No dual dialects, no in-memory fallback, no feature flags._

---

## 1. What this step is about

The dual-backend design was a hedge. We're removing the hedge. Two hard commitments:

1. **Postgres is the only storage backend** for everything MOA persists — session event log, context snapshots, approval rules, and the wiki memory search index.
2. **Docker is required for local development.** `docker-compose up -d` gives you a pgvector-enabled Postgres 16 on port 5432. MOA refuses to start without a reachable Postgres.

This removes:

| Footprint deleted | Size |
|---|---|
| `moa-session/src/turso.rs` | 38 KB |
| `moa-session/src/queries_turso.rs` | 11 KB |
| `moa-session/src/schema_turso.rs` | 4 KB |
| `moa-session/src/backend.rs` (enum dispatch) | 11 KB → replaced by a 30-line re-export |
| `moa-memory/src/fts.rs` (FTS5/libsql search) | 13 KB → rewritten to Postgres (step 90) |
| `libsql` dependency | removed from `moa-memory/Cargo.toml` |
| `#[cfg(feature = "turso")]` gates across workspace | ~20 sites removed |
| `DatabaseBackend::Turso` variant + routing code | ~200 LoC |
| Dual-dialect test fixtures | various |

Roughly **80 KB of code and one major dependency** gone. Replaced by: one `docker-compose.yml` and one `PostgresSessionStore` that everything uses directly.

What this step does NOT do:
- Does not implement the Postgres-native memory search (that's step 90).
- Does not implement Postgres LISTEN/NOTIFY event fanout (step 89).
- Does not add pgvector semantic search (step 91).
- Does not add analytic views / generated columns (step 92).

Those are the follow-on packs that exploit the simplification this step creates.

---

## 2. Files to read

**Session store:**
- `moa-session/src/lib.rs` — feature gates and re-exports.
- `moa-session/src/backend.rs` — enum dispatch; collapses to a trivial wrapper or goes away.
- `moa-session/src/{turso.rs, queries_turso.rs, schema_turso.rs}` — deleted.
- `moa-session/src/{postgres.rs, queries_postgres.rs, schema_postgres.rs}` — survive. File names drop the `_postgres` suffix.
- `moa-session/src/neon.rs` — Neon branching; stays.
- `moa-session/Cargo.toml` — drop Turso feature.

**Memory store:**
- `moa-memory/src/lib.rs` — uses `FtsIndex` from libsql.
- `moa-memory/src/fts.rs` — the 260-line FTS5 wrapper. Deleted in this step; replaced by a Postgres stub that step 90 fleshes out.
- `moa-memory/Cargo.toml` — drop `libsql`.

**Core + config:**
- `moa-core/src/config.rs` — `DatabaseBackend` enum, `LocalConfig.memory_dir`, etc.
- `moa-core/src/types/database.rs` (or wherever `DatabaseBackend` lives).

**Orchestrator + CLI:**
- `moa-orchestrator/src/local.rs` — holds `Arc<SessionDatabase>`.
- `moa-cli/src/main.rs` — startup wiring.

---

## 3. Goal

1. `moa-session` compiles against Postgres only. No `turso` feature. No `backend.rs` enum. `create_session_store(config)` returns `Arc<PostgresSessionStore>` (or `Arc<dyn SessionStore>` wrapping it — pick one).
2. `moa-memory` no longer depends on `libsql`. The FTS5 `search.db` file is gone. Search APIs exist as stubs that return `Err(MoaError::NotImplemented)` — real Postgres tsvector implementation lands in step 90.
3. `moa-core::DatabaseBackend` becomes either a deleted type or a single-variant enum kept for config-file compatibility. Pick deletion; simpler.
4. A single `docker-compose.yml` at repo root boots Postgres 16 + pgvector.
5. `moa init` writes a config with `DATABASE_URL=postgres://moa:moa@localhost:5432/moa`.
6. `moa` (any subcommand that needs persistence) refuses to start if Postgres isn't reachable, with a one-line fix instruction.
7. `moa doctor` tests the DB connection, reports the server version, and checks the pgvector extension is installed.
8. Every test path that previously used SQLite/tempfile or libsql now uses `testcontainers-modules::postgres::Postgres` or an already-running local Postgres.
9. `cargo build --release` produces a binary that does NOT link `libsqlite3` through any MOA crate.

---

## 4. Rules

- **No fallback.** If `DATABASE_URL` is missing, fail. If Postgres is unreachable, fail. No "degraded mode," no in-memory store, no ephemeral substitute. The operational model is: Docker is up, or nothing runs.
- **Startup checks the DB before anything else.** The orchestrator's first async action is a `SELECT 1` against Postgres. If it fails, the binary exits with an actionable error. This surfaces misconfiguration at boot, not at first user message.
- **Schema lives in one place.** Rename `schema_postgres.rs` → `schema.rs`, `queries_postgres.rs` → `queries.rs`, `postgres.rs` stays. The `_postgres` suffix existed only to distinguish from Turso; it's noise now.
- **One migration file is the source of truth.** If we were using `sqlx::migrate!()` against two dialects, collapse to one migration directory: `moa-session/migrations/*.sql` (Postgres only).
- **Delete, don't deprecate.** This is a pre-1.0 codebase. No `#[deprecated]` markers, no compatibility shims. Delete the code and move on.
- **Memory search is temporarily broken.** Between this step and step 90, `FileMemoryStore::search` returns `Err(MoaError::NotImplemented)`. That's fine — it's a two-step landing. Step 90 is small enough to land same-week.
- **docker-compose is dev-only.** Production still uses Neon + Fly.io, as documented in `moa/docs/11-v2-architecture.md`. The compose file carries a header comment making this explicit.

---

## 5. Tasks

### 5a. Delete Turso/SQLite files from `moa-session`

```
git rm moa-session/src/turso.rs
git rm moa-session/src/queries_turso.rs
git rm moa-session/src/schema_turso.rs
git mv moa-session/src/postgres.rs   moa-session/src/store.rs       # or keep as postgres.rs
git mv moa-session/src/queries_postgres.rs moa-session/src/queries.rs
git mv moa-session/src/schema_postgres.rs  moa-session/src/schema.rs
```

Edit `moa-session/src/lib.rs`:

```rust
//! Postgres session store for MOA.

pub mod blob;
pub mod neon;
pub mod queries;
pub mod schema;
pub mod store;

pub use blob::FileBlobStore;
pub use neon::NeonBranchManager;
pub use store::PostgresSessionStore;

/// Creates a shared session store from config.
pub async fn create_session_store(config: &MoaConfig) -> Result<Arc<PostgresSessionStore>> {
    let store = PostgresSessionStore::from_config(config).await?;
    store.ping().await?; // fail fast if Postgres is unreachable
    Ok(Arc::new(store))
}
```

### 5b. Delete `backend.rs`

Remove `moa-session/src/backend.rs`. The enum dispatch is no longer meaningful.

Any consumer that depended on `SessionDatabase` changes to `PostgresSessionStore` directly (or `Arc<dyn SessionStore>` if the consumer prefers trait objects). Both work. Pick direct-concrete for callers inside the MOA workspace; pick trait-object only at public boundaries where trait-object provides extension value.

`ApprovalRuleStore` is already a trait. `PostgresSessionStore` implements it directly. No dispatch-layer changes needed there.

### 5c. `PostgresSessionStore::from_config` + `ping`

Add a `ping()` method:

```rust
impl PostgresSessionStore {
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| MoaError::ConfigError(format!(
                "cannot reach Postgres at {}: {}. Run `docker-compose up -d` from the repo root, \
                 or set DATABASE_URL to a reachable Postgres instance.",
                redact_password(&self.url), e
            )))?;
        Ok(())
    }
}
```

Redact the password in error output. The host:port must appear (it's diagnostic information).

### 5d. Delete SQLite from `moa-memory`

1. Remove `libsql = ...` from `moa-memory/Cargo.toml`.
2. In `moa-memory/src/lib.rs`, remove the `FtsIndex` field from `FileMemoryStore`. Remove the `search.db` file creation in `FileMemoryStore::new`.
3. Delete `moa-memory/src/fts.rs`.
4. Add a new `moa-memory/src/search.rs` stub:

```rust
//! Wiki page search. Postgres tsvector + GIN implementation lands in step 90.

use moa_core::{MemoryPath, MemoryScope, MemorySearchResult, MoaError, Result, WikiPage};

#[derive(Clone)]
pub struct WikiSearchIndex {
    // Step 90 replaces this with an Arc<PgPool> + scoped queries.
}

impl WikiSearchIndex {
    pub fn new() -> Self { Self {} }

    pub async fn search(
        &self,
        _query: &str,
        _scope: &MemoryScope,
        _limit: usize,
    ) -> Result<Vec<MemorySearchResult>> {
        Err(MoaError::NotImplemented(
            "wiki search requires the Postgres tsvector index — see step 90".into(),
        ))
    }

    pub async fn upsert_page(
        &self,
        _scope: &MemoryScope,
        _path: &MemoryPath,
        _page: &WikiPage,
    ) -> Result<()> {
        // Step 90: write to wiki_pages with tsvector update.
        Ok(())
    }

    pub async fn delete_page(&self, _scope: &MemoryScope, _path: &MemoryPath) -> Result<()> {
        Ok(())
    }

    pub async fn rebuild_scope(
        &self,
        _scope: &MemoryScope,
        _pages: &[(MemoryPath, WikiPage)],
    ) -> Result<()> {
        Ok(())
    }
}
```

5. `FileMemoryStore::new` becomes:

```rust
pub async fn new(base_dir: impl AsRef<Path>) -> Result<Self> {
    let base_dir = base_dir.as_ref().to_path_buf();
    fs::create_dir_all(base_dir.join("memory")).await?;
    fs::create_dir_all(base_dir.join("workspaces")).await?;
    Ok(Self {
        base_dir: Arc::new(base_dir),
        search_index: WikiSearchIndex::new(),
    })
}
```

6. Wire the Postgres pool through a `from_config_with_pool(config, pool)` constructor that step 90 will use. For now the stubs don't need the pool.

### 5e. docker-compose.yml

At repo root:

```yaml
# docker-compose.yml — Required for local MOA development.
# Production uses Neon + Fly.io per moa/docs/11-v2-architecture.md.

services:
  postgres:
    image: pgvector/pgvector:pg16
    container_name: moa-postgres
    environment:
      POSTGRES_DB: moa
      POSTGRES_USER: moa
      POSTGRES_PASSWORD: moa
    ports:
      - "5432:5432"
    volumes:
      - moa-pg-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U moa -d moa"]
      interval: 5s
      timeout: 3s
      retries: 10
      start_period: 10s
    command:
      - postgres
      - -c
      - max_connections=200
      - -c
      - shared_buffers=256MB
      - -c
      - work_mem=16MB

volumes:
  moa-pg-data:
```

Add `make dev` / `make dev-down` targets in a `Makefile`:

```makefile
.PHONY: dev dev-down dev-logs

dev:
	docker-compose up -d
	@until docker-compose exec -T postgres pg_isready -U moa >/dev/null 2>&1; do \
	  echo "waiting for postgres..."; sleep 1; \
	done
	@echo "postgres ready on localhost:5432 (user=moa db=moa)"

dev-down:
	docker-compose down

dev-logs:
	docker-compose logs -f postgres
```

### 5f. Config simplification

`MoaConfig` loses the `database.backend` field entirely. `DATABASE_URL` becomes the single source of truth:

```rust
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,      // default 20
    pub connect_timeout_seconds: u64, // default 10
}
```

Delete `DatabaseBackend` enum from `moa-core`. Delete any config migration code that translated `backend = "turso"` to `backend = "postgres"`.

`moa init` writes:

```toml
[database]
url = "postgres://moa:moa@localhost:5432/moa"
max_connections = 20
```

`moa init --production` can accept `--database-url` and write a Neon URL.

### 5g. `moa doctor`

```rust
async fn doctor_database(config: &MoaConfig) -> DoctorReport {
    let store = match PostgresSessionStore::from_config(config).await {
        Ok(s) => s,
        Err(e) => return fail(format!("connect failed: {e}")),
    };
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&store.pool).await.map_err(...)?;
    let pgvector: Option<String> = sqlx::query_scalar(
        "SELECT extversion FROM pg_extension WHERE extname = 'vector'"
    ).fetch_optional(&store.pool).await?;

    ok(format!(
        "Postgres: {}\npgvector: {}",
        version.lines().next().unwrap_or("unknown"),
        pgvector.unwrap_or_else(|| "NOT INSTALLED".into())
    ))
}
```

If pgvector is missing, report it but don't fail — step 91 will need it; step 83 doesn't.

### 5h. Test infrastructure

All tests migrate to `testcontainers-modules::postgres::Postgres`:

```rust
// moa-session/tests/common.rs
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

pub async fn test_postgres_store() -> (PostgresSessionStore, ContainerAsync<Postgres>) {
    let container = Postgres::default().start().await.unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let store = PostgresSessionStore::from_url(&url, 10).await.unwrap();
    store.run_migrations().await.unwrap();
    (store, container)
}
```

Delete any `setup_turso_test_store` helpers. Delete `tempfile`-based DB test helpers.

For fast CI, consider a session-scoped container (testcontainers shared across tests) or share the dev docker-compose instance when `TEST_DATABASE_URL` is set.

### 5i. Stale data detection

If `~/.moa/sessions.db` OR `~/.moa/**/search.db` exists at startup, log **once** per file:

```
warning: Stale SQLite file detected at {path}.
         MOA 0.X+ uses Postgres only. This file is ignored and can be deleted.
```

Store a sentinel at `~/.moa/.sqlite-stale-ack` after first warning. Ship no migration tooling in this step; fresh-start is the expected path.

### 5j. Documentation updates

- `moa/docs/05-session-event-log.md`: remove Turso/libSQL language; replace with Postgres throughout.
- `moa/docs/04-memory-architecture.md`: FTS5 section rewritten to reference Postgres tsvector (forward-references step 90).
- `moa/docs/10-technology-stack.md`: drop Turso row; drop libsql; add docker-compose; add "Postgres 16 + pgvector required locally".
- `moa/docs/11-v2-architecture.md`: mark the Postgres-only decision as landed.
- `README.md`: new "Quickstart":
  ```
  # Start Postgres
  docker-compose up -d
  
  # Build and initialize
  cargo build
  cargo run -- init
  cargo run -- doctor
  
  # Run
  cargo run -- "hello"
  ```

### 5k. Workspace Cargo.toml cleanup

- Remove `libsql` from the workspace dependencies if it was pinned there.
- Remove `[features] turso = [...]` from `moa-session/Cargo.toml`.
- Remove `[features] default = ["postgres"]` — no feature gating at all.
- Bump `moa-session` version to signal the breaking change.

---

## 6. Deliverables

- [ ] `moa-session/src/{turso,queries_turso,schema_turso,backend}.rs` deleted.
- [ ] `postgres.rs` → `store.rs`; `queries_postgres.rs` → `queries.rs`; `schema_postgres.rs` → `schema.rs`.
- [ ] `moa-memory/src/fts.rs` deleted; `search.rs` stub added.
- [ ] `libsql` removed from `moa-memory/Cargo.toml`.
- [ ] `DatabaseBackend` enum and `database.backend` config field deleted.
- [ ] `PostgresSessionStore::ping()` runs at orchestrator startup; failure is fatal with actionable error.
- [ ] `docker-compose.yml` + `Makefile` shipped at repo root.
- [ ] `moa init` writes `DATABASE_URL` for local Docker Postgres.
- [ ] `moa doctor` reports Postgres version and pgvector presence.
- [ ] All `#[cfg(feature = "turso")]` removed workspace-wide.
- [ ] All tests migrated to testcontainers Postgres.
- [ ] Stale SQLite file detection with one-time warning.
- [ ] Docs 04, 05, 10, 11, README updated.
- [ ] `cargo build --release` produces a binary with no `libsqlite3` or `libsql` linkage anywhere in the MOA workspace.
- [ ] `cargo test --workspace` green (step 90 implements real search; `search` tests in `moa-memory` are temporarily `#[ignore]`d with a TODO referencing step 90).

---

## 7. Acceptance criteria

1. `grep -rn 'turso\|libsql\|feature = "turso"' .` returns **zero** matches in tracked source code (excluding this pack's historical docs).
2. `cargo tree -p moa-session` shows `sqlx` and `tokio-postgres` but no `libsql`, `rusqlite`, or `libsqlite3-sys`.
3. `cargo tree -p moa-memory` shows no SQLite-family dependencies.
4. `docker-compose down -v && docker-compose up -d && cargo run -- doctor` exits 0, reports Postgres 16 and pgvector present.
5. With Postgres stopped (`docker-compose down`), `cargo run -- exec "hello"` exits 1 with: `cannot reach Postgres at localhost:5432: ... Run docker-compose up -d ...`.
6. A freshly booted `make dev` Postgres completes session creation, a 2-turn exchange, and session reload within 3 seconds on a typical laptop.
7. The step 78 integration test passes against a testcontainers Postgres.
8. `moa-memory`'s search function returns `MoaError::NotImplemented` until step 90 lands; the integration test in step 78 does NOT exercise memory search (or `#[ignore]`s it with a pointer to step 90).
9. The binary size drops by at least 500 KB (eliminating libsql + SQLite + libsqlite3-sys).
10. Contributor onboarding docs: "1. Install Docker. 2. `make dev`. 3. `cargo run -- doctor`. 4. Go." No other storage setup.
