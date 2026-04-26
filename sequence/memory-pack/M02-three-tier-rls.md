# Step M02 — 3-tier scope routing in Postgres + RLS for Global tier

_Migrate every workspace-scoped table to a `scope` column model with FORCE-RLS policies that enforce Global-readable + Workspace-isolated + User-private at the row level, and harden Rust connection borrowing so `SET LOCAL moa.workspace_id` / `moa.user_id` / `moa.scope_tier` GUCs are always set._

## 1 What this step is about

M01 added `MemoryScope::Global` to the type system. This step makes Postgres enforce the three-tier model. Every existing workspace-scoped table gains a `scope TEXT` column with values `'global' | 'workspace' | 'user'`, an optional `user_id` column, and **FORCE row-level-security policies** that produce three behaviors:

- **Read**: any session sees Global rows + its own Workspace rows + its own User rows; never another user's User rows or another workspace's Workspace rows.
- **Write**: a session can only write rows tagged with its current `(workspace_id, user_id)` GUC values. Global writes are blocked from `moa_app` and require the `moa_promoter` role (separate concept used by promotion paths).
- **GUC discipline**: every connection borrowed from the pool runs `SET LOCAL moa.workspace_id = ...; SET LOCAL moa.user_id = ...; SET LOCAL moa.scope_tier = ...;` inside its outer transaction, or the query fails RLS.

This is a heavy migration but lands now because every later prompt (M03 AGE, M05 vector, M06 changelog) reuses the same RLS template.

## 2 Files to read

- All migration files under `migrations/` or `crates/moa-memory/migrations/`
- `crates/moa-runtime/src/db/pool.rs` (or wherever pg pool lives — confirm via `rg "PgPool" crates/`)
- `crates/moa-runtime/src/context.rs`
- `crates/moa-core/src/memory/scope.rs` (from M01)
- Any existing `CREATE POLICY` SQL in the codebase

## 3 Goal

1. Migration `M02_three_tier_rls.sql` adds:
   - `moa_app` and `moa_promoter` Postgres roles (both NOLOGIN; app connects via PgBouncer/connection-string mapping).
   - A canonical `scope_tier` enum-like CHECK pattern.
   - Drop of the old workspace-only RLS policies.
   - New 3-tier policies on every workspace-scoped table.

2. Rust pool wrapper enforces `SET LOCAL` GUCs on every borrowed connection. A connection used without GUCs panics in debug builds.

3. New runtime helper `ScopeContext` carrying `(workspace_id, user_id, scope_tier)` is plumbed from request to query.

4. Pre-3-tier RLS policies are deleted (cleanup).

## 4 Rules

- **FORCE row level security** on every tenant table — `ALTER TABLE … FORCE ROW LEVEL SECURITY` so even table owners are subject to RLS during normal operation.
- **`moa_app` role is NEVER `BYPASSRLS`.** Migrations run as `moa_owner` (separate role with `NOBYPASSRLS` removed for migrations only).
- **GUCs use `pg_catalog.set_config(name, value, true)`** when running through PgBouncer in transaction mode. (Direct `SET LOCAL` works in session mode but fails silently in transaction mode after the first transaction.)
- **No `BYPASSRLS` anywhere in production code paths.** The cross-tenant pen-test in M25 will fail if any consumer sneaks BYPASS in.
- **`scope_tier` is computed, not free-form.** It must equal `'global'` when `workspace_id IS NULL`, `'user'` when both `workspace_id` and `user_id` are set, otherwise `'workspace'`.

## 5 Tasks

### 5a Migration: `migrations/M02_three_tier_rls.sql`

```sql
-- Roles
CREATE ROLE moa_app NOLOGIN;
CREATE ROLE moa_promoter NOLOGIN;
CREATE ROLE moa_owner NOLOGIN;
CREATE ROLE moa_auditor NOLOGIN;

-- Migrations execute as moa_owner; app sessions inherit moa_app.

-- Helper to compute scope_tier deterministically.
CREATE OR REPLACE FUNCTION moa.compute_scope_tier(
    workspace_id UUID, user_id UUID
) RETURNS TEXT
LANGUAGE SQL IMMUTABLE
AS $$
    SELECT CASE
        WHEN workspace_id IS NULL AND user_id IS NULL THEN 'global'
        WHEN workspace_id IS NOT NULL AND user_id IS NOT NULL THEN 'user'
        WHEN workspace_id IS NOT NULL AND user_id IS NULL THEN 'workspace'
        ELSE NULL  -- (NULL, not-NULL) is illegal
    END;
$$;

-- Helper to read current GUC values (returns NULL if unset).
CREATE OR REPLACE FUNCTION moa.current_workspace() RETURNS UUID
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.workspace_id', TRUE), '')::UUID;
$$;

CREATE OR REPLACE FUNCTION moa.current_user_id() RETURNS UUID
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.user_id', TRUE), '')::UUID;
$$;

CREATE OR REPLACE FUNCTION moa.current_scope_tier() RETURNS TEXT
LANGUAGE SQL STABLE
AS $$
    SELECT NULLIF(current_setting('moa.scope_tier', TRUE), '');
$$;
```

### 5b Drop old policies

For every existing tenant-scoped table, drop pre-3-tier policies. List them by:

```sh
psql -d moa -c "SELECT schemaname, tablename, policyname FROM pg_policies WHERE schemaname = 'moa';"
```

Then in the migration:

```sql
DROP POLICY IF EXISTS workspace_isolation ON moa.session_event;
DROP POLICY IF EXISTS workspace_isolation ON moa.skills;
-- ... etc, one per table that existed pre-M02
```

### 5c Add scope columns to existing tables

For each existing tenant-scoped table that does not yet have `scope`/`user_id`:

```sql
ALTER TABLE moa.session_event
  ADD COLUMN IF NOT EXISTS user_id UUID,
  ADD COLUMN IF NOT EXISTS scope TEXT
    GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED;

CREATE INDEX IF NOT EXISTS session_event_scope_idx
  ON moa.session_event (workspace_id, scope, user_id);
```

(Repeat for every workspace-scoped table identified in the audit; expect ~5–8 tables.)

### 5d Apply 3-tier policy template

For every tenant-scoped table:

```sql
ALTER TABLE moa.session_event ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.session_event FORCE ROW LEVEL SECURITY;

-- READ: global is visible to all; workspace+user filtered by GUCs.
CREATE POLICY rd_global ON moa.session_event FOR SELECT TO moa_app
  USING (scope = 'global');

CREATE POLICY rd_workspace ON moa.session_event FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());

CREATE POLICY rd_user ON moa.session_event FOR SELECT TO moa_app
  USING (scope = 'user'
         AND workspace_id = moa.current_workspace()
         AND user_id = moa.current_user_id());

-- WRITE: app cannot insert/update/delete global rows; promoter role can.
CREATE POLICY wr_workspace ON moa.session_event FOR ALL TO moa_app
  USING      (scope = 'workspace' AND workspace_id = moa.current_workspace())
  WITH CHECK (scope = 'workspace' AND workspace_id = moa.current_workspace());

CREATE POLICY wr_user ON moa.session_event FOR ALL TO moa_app
  USING      (scope = 'user'
              AND workspace_id = moa.current_workspace()
              AND user_id = moa.current_user_id())
  WITH CHECK (scope = 'user'
              AND workspace_id = moa.current_workspace()
              AND user_id = moa.current_user_id());

CREATE POLICY wr_global_promoter ON moa.session_event FOR ALL TO moa_promoter
  USING (scope = 'global') WITH CHECK (scope = 'global');

GRANT SELECT, INSERT, UPDATE, DELETE ON moa.session_event TO moa_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON moa.session_event TO moa_promoter;
```

### 5e Rust: GUC-enforced connection wrapper

In `crates/moa-runtime/src/db/scoped_conn.rs`:

```rust
use sqlx::{PgPool, PgConnection, Postgres, Transaction};
use crate::ScopeContext;

pub struct ScopedConn<'p> { tx: Transaction<'p, Postgres> }

impl<'p> ScopedConn<'p> {
    pub async fn begin(pool: &'p PgPool, ctx: &ScopeContext) -> Result<Self> {
        let mut tx = pool.begin().await?;
        Self::apply_gucs(&mut tx, ctx).await?;
        Ok(Self { tx })
    }

    async fn apply_gucs(tx: &mut Transaction<'_, Postgres>, ctx: &ScopeContext) -> Result<()> {
        let workspace = ctx.workspace_id().map(|w| w.to_string()).unwrap_or_default();
        let user      = ctx.user_id().map(|u| u.to_string()).unwrap_or_default();
        let tier      = ctx.tier_str();   // "global" | "workspace" | "user"
        sqlx::query!("SELECT pg_catalog.set_config('moa.workspace_id', $1, true)", workspace).execute(&mut **tx).await?;
        sqlx::query!("SELECT pg_catalog.set_config('moa.user_id',      $1, true)", user).execute(&mut **tx).await?;
        sqlx::query!("SELECT pg_catalog.set_config('moa.scope_tier',   $1, true)", tier).execute(&mut **tx).await?;
        Ok(())
    }

    pub fn as_mut(&mut self) -> &mut PgConnection { &mut *self.tx }

    pub async fn commit(self) -> Result<()> { self.tx.commit().await?; Ok(()) }
}
```

In `crates/moa-runtime/src/db/pool.rs`, replace direct `pool.acquire()` / `pool.begin()` calls in tenant-context paths with `ScopedConn::begin(&pool, &ctx)`. Add a `#[cfg(debug_assertions)]` panic guard:

```rust
#[cfg(debug_assertions)]
async fn assert_no_naked_acquire(pool: &PgPool) {
    // optional: use a sqlx-instrumentation hook to assert that any tenant-table query
    // ran inside a ScopedConn — see crates/moa-runtime/src/db/instrumentation.rs
}
```

### 5f Plumb `ScopeContext` from request to query

`ScopeContext` already exists conceptually after M01 (`MemoryScope` + identifiers). Wrap it:

```rust
#[derive(Debug, Clone)]
pub struct ScopeContext { scope: MemoryScope }

impl ScopeContext {
    pub fn new(scope: MemoryScope) -> Self { Self { scope } }
    pub fn scope(&self) -> &MemoryScope { &self.scope }
    pub fn workspace_id(&self) -> Option<WorkspaceId> { self.scope.workspace_id() }
    pub fn user_id(&self)      -> Option<UserId>      { self.scope.user_id() }
    pub fn tier_str(&self) -> &'static str {
        match self.scope.tier() {
            ScopeTier::Global => "global",
            ScopeTier::Workspace => "workspace",
            ScopeTier::User => "user",
        }
    }
}
```

Pass through every gateway → orchestrator → memory call site. Any function that today takes `WorkspaceId` should take `&ScopeContext` if it reads/writes Postgres tenant tables.

## 6 Deliverables

- `migrations/M02_three_tier_rls.sql` (~250 lines).
- `crates/moa-runtime/src/db/scoped_conn.rs` (~120 lines).
- `crates/moa-runtime/src/scope_context.rs` (~80 lines).
- Updated call sites across moa-runtime, moa-orchestrator, moa-brain.

## 7 Acceptance criteria

1. `cargo build --workspace` clean.
2. Migration applies cleanly to a fresh `postgres:17.6` container with no errors.
3. `psql -d moa -U moa_app -c "SELECT * FROM moa.session_event"` returns 0 rows when no GUCs set.
4. With `SET LOCAL moa.workspace_id = '<wid>'; SET LOCAL moa.scope_tier = 'workspace';`, the same query returns workspace's own rows + global rows.
5. With `SET LOCAL moa.scope_tier = 'user'; SET LOCAL moa.user_id = '<uid>';`, the query also returns own user rows.
6. INSERT into `scope='global'` from `moa_app` is rejected; from `moa_promoter` is accepted.
7. Rust integration test asserts cross-tenant SELECT returns 0 rows.

## 8 Tests

```sh
docker compose up -d postgres
cargo run --bin migrate
cargo test -p moa-runtime --test rls_three_tier
```

Manual smoke (replace UUIDs):

```sql
SET LOCAL ROLE moa_app;
SET LOCAL moa.workspace_id = '<A>';
SET LOCAL moa.scope_tier   = 'workspace';
SELECT count(*) FROM moa.session_event WHERE workspace_id = '<B>';   -- expect 0
INSERT INTO moa.session_event (workspace_id, scope, payload) VALUES ('<B>', 'workspace', '{}'::jsonb);  -- expect violation
INSERT INTO moa.session_event (workspace_id, scope, payload) VALUES (NULL, 'global', '{}'::jsonb);     -- expect violation (moa_app cannot write global)
```

## 9 Cleanup

- **Delete** all pre-3-tier `CREATE POLICY` statements in earlier migration files (or mark them dropped via `DROP POLICY IF EXISTS` — preferred for safety).
- **Delete** any Rust code that called `SET app.workspace_id` (old GUC name); the new GUC is `moa.workspace_id`.
- **Delete** any helper `pg_session_workspace()` SQL function from the old single-tier era; replaced by `moa.current_workspace()`.
- Remove the `// TODO(M02)` comments left by M01 in `moa-brain/src/pipeline/memory_retriever.rs` etc., since this prompt resolves them — those sites now use `scope.ancestors()` for routing and the DB enforces isolation.

## 10 What's next

**M03 — Apache AGE installation + bootstrap migration**. With Postgres + RLS + 3-tier scope in place, install AGE and create the graph (vlabels, elabels, indexes).
