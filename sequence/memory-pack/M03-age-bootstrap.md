# Step M03 — Apache AGE installation + bootstrap migration

_Install Apache AGE 1.7.0 on Postgres 17.6+, create the `moa_graph` graph, define the seven vertex labels and nine edge labels, and create the BTREE/GIN/expression indexes that make AGE fast enough for the hot path._

## 1 What this step is about

AGE is a Postgres extension that adds openCypher graph support. AGE has no native label or property indexes — every label is a heap table under the schema-of-the-graph (e.g., `moa_graph."Entity"`), and we must create indexes manually using `agtype_access_operator` for property paths. We also enable AGE's first-class RLS support (PR #2309, shipped in 1.7.0). This prompt is mechanical but every line matters: missing an index turns a 5ms Cypher into a 5s sequential scan over millions of rows.

## 2 Files to read

- `docker-compose.yml` (local dev stack)
- `migrations/` directory (for naming convention)
- M00 stack-pin table (PG 17.6+, AGE release/PG17/1.7.0)
- M02 RLS template (we reuse it for AGE label tables)

## 3 Goal

1. AGE 1.7.0 installed in the local dev `postgres:17.6` image and in the production base image.
2. Migration `M03_age_bootstrap.sql` creates the graph, all seven vertex labels, all nine edge labels, every required index, and applies FORCE-RLS to every AGE label table.
3. Smoke test: a single Cypher MATCH/CREATE round-trip succeeds from a Rust integration test.

## 4 Rules

- **AGE branch**: pin to `release/PG17/1.7.0`. Do not use `master` — it tracks PG18 and breaks our build.
- **`SET search_path` discipline**: every connection that runs Cypher must have `SET search_path = ag_catalog, "$user", public;` set in the same transaction. The `ScopedConn` from M02 is updated in this prompt to do that automatically.
- **Property indexes use `agtype_access_operator`**, not direct property path syntax. Get this wrong and the planner will not use the index.
- **Every AGE label table inherits the M02 RLS template** verbatim (read=global+workspace+user, write=workspace+user). Property `workspace_id`, `user_id`, `scope` live inside the agtype JSON; we extract them via expression indexes.
- **No multi-graph**: one graph named `moa_graph` per database. Multi-tenancy is property-based, not graph-based.

## 5 Tasks

### 5a Update docker-compose.yml

```yaml
services:
  postgres:
    image: apache/age:PG17_latest    # tracks release/PG17 branch
    environment:
      POSTGRES_DB: moa
      POSTGRES_USER: moa_owner
      POSTGRES_PASSWORD: dev
    command:
      - "postgres"
      - "-c"
      - "shared_preload_libraries=age,pgaudit"
      - "-c"
      - "session_preload_libraries=age"
    ports: ["5432:5432"]
    volumes:
      - moa_pg:/var/lib/postgresql/data
volumes:
  moa_pg: {}
```

(If using a self-built image, document the build steps in `docs/ops/build-pg-age-image.md`. The Apache AGE Dockerfile against `postgres:17.6-bookworm` is available in `apache/age` repo.)

### 5b Migration: `migrations/M03_age_bootstrap.sql`

```sql
-- Extension
CREATE EXTENSION IF NOT EXISTS age;
LOAD 'age';
SET search_path = ag_catalog, "$user", public;

-- Graph
SELECT * FROM ag_catalog.create_graph('moa_graph');

-- Vertex labels (7)
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Entity');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Concept');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Decision');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Incident');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Lesson');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Fact');
SELECT * FROM ag_catalog.create_vlabel('moa_graph','Source');

-- Edge labels (9)
SELECT * FROM ag_catalog.create_elabel('moa_graph','RELATES_TO');
SELECT * FROM ag_catalog.create_elabel('moa_graph','DEPENDS_ON');
SELECT * FROM ag_catalog.create_elabel('moa_graph','SUPERSEDES');
SELECT * FROM ag_catalog.create_elabel('moa_graph','CONTRADICTS');
SELECT * FROM ag_catalog.create_elabel('moa_graph','DERIVED_FROM');
SELECT * FROM ag_catalog.create_elabel('moa_graph','MENTIONED_IN');
SELECT * FROM ag_catalog.create_elabel('moa_graph','CAUSED');
SELECT * FROM ag_catalog.create_elabel('moa_graph','LEARNED_FROM');
SELECT * FROM ag_catalog.create_elabel('moa_graph','APPLIES_TO');

-- Per-vertex-label indexes
DO $$
DECLARE lbl TEXT;
BEGIN
    FOR lbl IN SELECT unnest(ARRAY['Entity','Concept','Decision','Incident','Lesson','Fact','Source']) LOOP
        -- BTREE on the always-present id column
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_id_idx ON moa_graph.%I USING BTREE (id)', lbl, lbl);
        -- Expression indexes on hot properties: uid, workspace_id, scope, valid_to
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_uid_idx ON moa_graph.%I USING BTREE
            ((agtype_access_operator(VARIADIC ARRAY[properties, ''"uid"''::agtype])))',
            lbl, lbl);
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_workspace_idx ON moa_graph.%I USING BTREE
            ((agtype_access_operator(VARIADIC ARRAY[properties, ''"workspace_id"''::agtype])))',
            lbl, lbl);
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_scope_idx ON moa_graph.%I USING BTREE
            ((agtype_access_operator(VARIADIC ARRAY[properties, ''"scope"''::agtype])))',
            lbl, lbl);
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_validto_partial_idx ON moa_graph.%I USING BTREE
            ((agtype_access_operator(VARIADIC ARRAY[properties, ''"valid_to"''::agtype])))
            WHERE (agtype_access_operator(VARIADIC ARRAY[properties, ''"valid_to"''::agtype])) IS NULL',
            lbl, lbl);
        -- GIN over whole properties for ad-hoc filters
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_props_gin ON moa_graph.%I USING GIN (properties)',
            lbl, lbl);
    END LOOP;
END $$;

-- Per-edge-label indexes (start_id / end_id)
DO $$
DECLARE lbl TEXT;
BEGIN
    FOR lbl IN SELECT unnest(ARRAY['RELATES_TO','DEPENDS_ON','SUPERSEDES','CONTRADICTS','DERIVED_FROM','MENTIONED_IN','CAUSED','LEARNED_FROM','APPLIES_TO']) LOOP
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_start_idx ON moa_graph.%I USING BTREE (start_id)', lbl, lbl);
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_end_idx   ON moa_graph.%I USING BTREE (end_id)',   lbl, lbl);
        EXECUTE format('CREATE INDEX IF NOT EXISTS %I_workspace_idx ON moa_graph.%I USING BTREE
            ((agtype_access_operator(VARIADIC ARRAY[properties, ''"workspace_id"''::agtype])))',
            lbl, lbl);
    END LOOP;
END $$;

-- RLS on every AGE label table (FORCE)
DO $$
DECLARE lbl TEXT;
BEGIN
    FOR lbl IN SELECT unnest(ARRAY['Entity','Concept','Decision','Incident','Lesson','Fact','Source',
                                    'RELATES_TO','DEPENDS_ON','SUPERSEDES','CONTRADICTS','DERIVED_FROM',
                                    'MENTIONED_IN','CAUSED','LEARNED_FROM','APPLIES_TO']) LOOP
        EXECUTE format('ALTER TABLE moa_graph.%I ENABLE ROW LEVEL SECURITY', lbl);
        EXECUTE format('ALTER TABLE moa_graph.%I FORCE  ROW LEVEL SECURITY', lbl);

        -- Read: global rows readable by all; workspace/user rows GUC-filtered.
        EXECUTE format($f$
            CREATE POLICY rd_global ON moa_graph.%I FOR SELECT TO moa_app
              USING (agtype_access_operator(VARIADIC ARRAY[properties, '"scope"'::agtype])
                     = '"global"'::agtype)
        $f$, lbl);
        EXECUTE format($f$
            CREATE POLICY rd_workspace ON moa_graph.%I FOR SELECT TO moa_app
              USING (agtype_access_operator(VARIADIC ARRAY[properties, '"scope"'::agtype])
                     = '"workspace"'::agtype
                 AND agtype_access_operator(VARIADIC ARRAY[properties, '"workspace_id"'::agtype])::TEXT
                     = ('"' || moa.current_workspace()::TEXT || '"'))
        $f$, lbl);
        EXECUTE format($f$
            CREATE POLICY rd_user ON moa_graph.%I FOR SELECT TO moa_app
              USING (agtype_access_operator(VARIADIC ARRAY[properties, '"scope"'::agtype])
                     = '"user"'::agtype
                 AND agtype_access_operator(VARIADIC ARRAY[properties, '"workspace_id"'::agtype])::TEXT
                     = ('"' || moa.current_workspace()::TEXT || '"')
                 AND agtype_access_operator(VARIADIC ARRAY[properties, '"user_id"'::agtype])::TEXT
                     = ('"' || moa.current_user_id()::TEXT || '"'))
        $f$, lbl);

        -- Write: app can write workspace + user (matching GUCs); promoter writes global.
        EXECUTE format($f$
            CREATE POLICY wr_workspace_user ON moa_graph.%I FOR ALL TO moa_app
              USING      (agtype_access_operator(VARIADIC ARRAY[properties, '"workspace_id"'::agtype])::TEXT
                          = ('"' || moa.current_workspace()::TEXT || '"'))
              WITH CHECK (agtype_access_operator(VARIADIC ARRAY[properties, '"workspace_id"'::agtype])::TEXT
                          = ('"' || moa.current_workspace()::TEXT || '"'))
        $f$, lbl);
        EXECUTE format($f$
            CREATE POLICY wr_global ON moa_graph.%I FOR ALL TO moa_promoter
              USING      (agtype_access_operator(VARIADIC ARRAY[properties, '"scope"'::agtype])
                          = '"global"'::agtype)
              WITH CHECK (agtype_access_operator(VARIADIC ARRAY[properties, '"scope"'::agtype])
                          = '"global"'::agtype)
        $f$, lbl);

        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON moa_graph.%I TO moa_app', lbl);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON moa_graph.%I TO moa_promoter', lbl);
    END LOOP;
END $$;
```

### 5c Update `ScopedConn` to set search_path

In `crates/moa-runtime/src/db/scoped_conn.rs::apply_gucs`, append after the three GUC calls:

```rust
sqlx::query!("SET LOCAL search_path = ag_catalog, public").execute(&mut **tx).await?;
```

### 5d Rust smoke test

Create `crates/moa-runtime/tests/age_smoke.rs`:

```rust
#[tokio::test]
async fn age_round_trip() {
    let pool = test_pool().await;
    let ctx = ScopeContext::new(MemoryScope::Workspace { workspace_id: WorkspaceId::new() });
    let mut sc = ScopedConn::begin(&pool, &ctx).await.unwrap();
    sqlx::query("SELECT * FROM cypher('moa_graph', $$ CREATE (n:Entity {uid:'u1', workspace_id:'wid', scope:'workspace'}) RETURN n $$) AS (n agtype)")
        .execute(sc.as_mut()).await.unwrap();
    let row: (sqlx::types::JsonValue,) = sqlx::query_as("SELECT * FROM cypher('moa_graph', $$ MATCH (n:Entity {uid:'u1'}) RETURN n.uid $$) AS (uid agtype)")
        .fetch_one(sc.as_mut()).await.unwrap();
    assert!(row.0.to_string().contains("u1"));
    sc.commit().await.unwrap();
}
```

(In the test, `wid` and `'<uid>'` strings must match what was set into the GUC. The smoke test bypasses RLS by running as `moa_owner` — separate role for testing the round-trip itself; M25 has full RLS attack tests.)

## 6 Deliverables

- Updated `docker-compose.yml` (~30 lines).
- `migrations/M03_age_bootstrap.sql` (~250 lines).
- `crates/moa-runtime/src/db/scoped_conn.rs` updated for search_path.
- `crates/moa-runtime/tests/age_smoke.rs`.
- `docs/ops/age-installation.md` short note about base image.

## 7 Acceptance criteria

1. `docker compose down -v && docker compose up -d postgres && cargo run --bin migrate` applies cleanly.
2. `psql -d moa -c "SELECT count(*) FROM ag_catalog.ag_label WHERE graph = 'moa_graph'::regclass::oid"` returns at least 16 (7 vlabels + 9 elabels).
3. The age_smoke test passes.
4. `pg_indexes` shows the expected per-label expression indexes (`Entity_uid_idx`, etc.).
5. As `moa_app` with no GUCs, `SELECT * FROM cypher('moa_graph', $$ MATCH (n) RETURN n LIMIT 1 $$) AS (n agtype)` returns 0 rows after seeding workspace data — RLS catches it.

## 8 Tests

```sh
docker compose down -v
docker compose up -d postgres
cargo run --bin migrate
cargo test -p moa-runtime --test age_smoke
psql -d moa -U moa_owner -f scripts/verify_age_indexes.sql
```

## 9 Cleanup

- **No prior AGE state to delete** (this is the introduction).
- **Remove any leftover comments** from M01/M02 that referenced "TODO: AGE not yet installed" — they're resolved here.
- **Confirm no migration file references the old wiki schema** (search `MEMORY.md`, `_log.md` in `migrations/` — none should remain). Delete any stragglers.

## 10 What's next

**M04 — Sidecar projection table `moa.node_index`**. AGE's per-label heaps are great for storage but slow for joins; the sidecar is the hot-path lookup table.
