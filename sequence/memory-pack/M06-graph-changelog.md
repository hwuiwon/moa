# Step M06 — `graph_changelog` outbox table + Debezium configuration

_Create the immutable, append-only audit log of every graph mutation, with HIPAA-grade retention semantics, and stand up the Debezium PostgreSQL CDC v2 connector that ships changes downstream._

## 1 What this step is about

Every graph mutation (create/update/supersede/invalidate/erase) writes a row to `moa.graph_changelog` in the same transaction. Postgres logical replication, surfaced through Debezium PG CDC V2 (3.5.x), tails the changelog and publishes events to a Kafka topic. Three downstream consumers will use this feed: external CDC bridges (M20), audit shipping to S3 Object Lock (M22), and cache invalidation (M17). The changelog also doubles as the data source for GDPR Art. 15 export (M23) and audit-preserving redaction in M24.

## 2 Files to read

- M00 stack-pin (PG ≥17.6, Debezium PG CDC V2 3.5.x)
- M02 RLS template
- M22 plan (pgaudit + S3 Object Lock — for retention coordination)

## 3 Goal

1. `moa.graph_changelog` table with FORCE-RLS, no UPDATE/DELETE grants (append-only).
2. `wal_level = logical` and a publication `moa_changelog_pub` covering the changelog table.
3. Replication slot `moa_changelog_slot` reserved.
4. Debezium connector config in `ops/debezium/moa-changelog-connector.json`.
5. A `moa.workspace_state` table tracking per-workspace `changelog_version` for cache invalidation.
6. Trigger or in-app writer that bumps `workspace_state.changelog_version` on every changelog row.

## 4 Rules

- **Append-only**: `REVOKE UPDATE, DELETE, TRUNCATE` from every role (including `moa_owner` for application paths; only superuser can prune partitions during retention rollover).
- **Partition by month** for retention rollover: `PARTITION BY RANGE (created_at)`; `pg_partman` recommended for automation but not required.
- **6-year retention for HIPAA** documentation logs (45 CFR 164.316(b)(2)(i)). Active partitions live on disk; older partitions ship to S3 Object Lock (M22) and detach.
- **Immutable redaction**: erasure (M24) does NOT delete changelog rows. It writes a *new* row with `op='erase'` and a redacted `payload` (replacing PHI/PII with hashes). The original row stays for chain-of-custody.
- **`payload JSONB`** carries the serialized before/after — encrypted via M21 envelope when PHI/restricted, otherwise plain.
- **Workspace-version bump is in the same Postgres transaction** as the changelog INSERT. Cache invalidation (M17) keys on this version.

## 5 Tasks

### 5a Migration: `migrations/M06_graph_changelog.sql`

```sql
-- Partitioned changelog
CREATE TABLE moa.graph_changelog (
    change_id        BIGSERIAL,
    workspace_id     UUID,
    user_id          UUID,
    scope            TEXT NOT NULL,
    actor_id         UUID,                            -- principal who triggered
    actor_kind       TEXT NOT NULL CHECK (actor_kind IN ('user','agent','system','promoter','admin')),
    op               TEXT NOT NULL CHECK (op IN ('create','update','supersede','invalidate','erase','crypto_shred')),
    target_kind      TEXT NOT NULL CHECK (target_kind IN ('node','edge')),
    target_label     TEXT NOT NULL,
    target_uid       UUID NOT NULL,
    payload          JSONB NOT NULL,                  -- before/after; may be encrypted
    redaction_marker TEXT,                            -- non-null after erase
    pii_class        TEXT NOT NULL DEFAULT 'none',
    audit_metadata   JSONB,                           -- approval token jti, reason, etc.
    cause_change_id  BIGINT,                          -- FK-style; supersession chain
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (change_id, created_at)
) PARTITION BY RANGE (created_at);

-- 13 monthly partitions (1 current + 12 history); script creates more via pg_partman if installed
DO $$
DECLARE month_start DATE := date_trunc('month', now())::DATE - INTERVAL '12 months';
        i INT;
BEGIN
    FOR i IN 0..13 LOOP
        EXECUTE format(
          'CREATE TABLE IF NOT EXISTS moa.graph_changelog_%s PARTITION OF moa.graph_changelog
           FOR VALUES FROM (%L) TO (%L)',
          to_char(month_start + (i || ' months')::interval, 'YYYY_MM'),
          (month_start + (i || ' months')::interval)::DATE,
          (month_start + ((i+1) || ' months')::interval)::DATE);
    END LOOP;
END $$;

CREATE INDEX changelog_ws_idx        ON moa.graph_changelog (workspace_id, created_at DESC);
CREATE INDEX changelog_target_uid_idx ON moa.graph_changelog (target_uid);
CREATE INDEX changelog_actor_idx     ON moa.graph_changelog (actor_id) WHERE actor_id IS NOT NULL;
CREATE INDEX changelog_op_idx        ON moa.graph_changelog (op);
CREATE INDEX changelog_cause_idx     ON moa.graph_changelog (cause_change_id) WHERE cause_change_id IS NOT NULL;

-- RLS
ALTER TABLE moa.graph_changelog ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.graph_changelog FORCE ROW LEVEL SECURITY;

CREATE POLICY rd_global ON moa.graph_changelog FOR SELECT TO moa_app USING (scope = 'global');
CREATE POLICY rd_workspace ON moa.graph_changelog FOR SELECT TO moa_app
  USING (scope = 'workspace' AND workspace_id = moa.current_workspace());
CREATE POLICY rd_user ON moa.graph_changelog FOR SELECT TO moa_app
  USING (scope = 'user' AND workspace_id = moa.current_workspace() AND user_id = moa.current_user_id());

CREATE POLICY rd_auditor ON moa.graph_changelog FOR SELECT TO moa_auditor USING (true);

CREATE POLICY ins_app ON moa.graph_changelog FOR INSERT TO moa_app
  WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY ins_promoter ON moa.graph_changelog FOR INSERT TO moa_promoter
  WITH CHECK (scope = 'global');

-- Append-only: NO UPDATE, DELETE policies; revoke explicitly
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM PUBLIC;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_app;
REVOKE UPDATE, DELETE, TRUNCATE ON moa.graph_changelog FROM moa_promoter;

GRANT SELECT, INSERT ON moa.graph_changelog TO moa_app;
GRANT SELECT, INSERT ON moa.graph_changelog TO moa_promoter;
GRANT SELECT ON moa.graph_changelog TO moa_auditor;

-- Workspace state for cache invalidation
CREATE TABLE moa.workspace_state (
    workspace_id      UUID PRIMARY KEY,
    changelog_version BIGINT NOT NULL DEFAULT 0,
    vector_backend        TEXT NOT NULL DEFAULT 'pgvector' CHECK (vector_backend IN ('pgvector','turbopuffer')),
    vector_backend_state  TEXT NOT NULL DEFAULT 'steady'    CHECK (vector_backend_state IN ('steady','migrating','dual_read')),
    dual_read_until       TIMESTAMPTZ,
    hipaa_tier            TEXT NOT NULL DEFAULT 'standard'  CHECK (hipaa_tier IN ('standard','hipaa','restricted')),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE moa.workspace_state ENABLE ROW LEVEL SECURITY;
ALTER TABLE moa.workspace_state FORCE ROW LEVEL SECURITY;
CREATE POLICY ws_self ON moa.workspace_state FOR ALL TO moa_app
  USING (workspace_id = moa.current_workspace())
  WITH CHECK (workspace_id = moa.current_workspace());
CREATE POLICY ws_admin ON moa.workspace_state FOR ALL TO moa_promoter USING (true) WITH CHECK (true);
GRANT SELECT, INSERT, UPDATE ON moa.workspace_state TO moa_app;

-- Logical replication setup
ALTER SYSTEM SET wal_level = 'logical';
-- (requires restart; document in runbook)

CREATE PUBLICATION moa_changelog_pub FOR TABLE moa.graph_changelog;
SELECT pg_create_logical_replication_slot('moa_changelog_slot', 'pgoutput');
```

(NB: `wal_level=logical` requires a restart. Document this in the migration runbook; CI test brings up the container with the setting baked in.)

### 5b Postgres setting in docker-compose

Update `docker-compose.yml`:

```yaml
command:
  - "postgres"
  - "-c"
  - "shared_preload_libraries=age,pgaudit"
  - "-c"
  - "wal_level=logical"
  - "-c"
  - "max_replication_slots=10"
  - "-c"
  - "max_wal_senders=10"
```

### 5c Rust outbox writer

`crates/moa-memory-graph/src/changelog.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangelogRecord {
    pub workspace_id: Option<Uuid>,
    pub user_id: Option<Uuid>,
    pub scope: String,
    pub actor_id: Option<Uuid>,
    pub actor_kind: String,
    pub op: String,
    pub target_kind: String,
    pub target_label: String,
    pub target_uid: Uuid,
    pub payload: serde_json::Value,
    pub pii_class: String,
    pub audit_metadata: Option<serde_json::Value>,
    pub cause_change_id: Option<i64>,
}

pub async fn write_and_bump(
    conn: &mut sqlx::PgConnection, rec: ChangelogRecord,
) -> anyhow::Result<i64> {
    let row = sqlx::query!(
        r#"INSERT INTO moa.graph_changelog
           (workspace_id, user_id, scope, actor_id, actor_kind, op, target_kind, target_label,
            target_uid, payload, pii_class, audit_metadata, cause_change_id)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
           RETURNING change_id"#,
        rec.workspace_id, rec.user_id, rec.scope, rec.actor_id, rec.actor_kind,
        rec.op, rec.target_kind, rec.target_label, rec.target_uid,
        rec.payload, rec.pii_class, rec.audit_metadata, rec.cause_change_id,
    ).fetch_one(&mut *conn).await?;

    if let Some(ws) = rec.workspace_id {
        sqlx::query!(
            r#"INSERT INTO moa.workspace_state (workspace_id, changelog_version)
               VALUES ($1, 1)
               ON CONFLICT (workspace_id) DO UPDATE
                 SET changelog_version = moa.workspace_state.changelog_version + 1,
                     updated_at = now()"#,
            ws,
        ).execute(&mut *conn).await?;
    }

    Ok(row.change_id)
}
```

### 5d Debezium connector config

`ops/debezium/moa-changelog-connector.json`:

```json
{
  "name": "moa-changelog",
  "config": {
    "connector.class": "io.debezium.connector.postgresql.PostgresConnector",
    "tasks.max": "1",
    "database.hostname": "postgres",
    "database.port": "5432",
    "database.user": "moa_replicator",
    "database.password": "${file:/run/secrets/moa_replicator_pwd:password}",
    "database.dbname": "moa",
    "topic.prefix": "moa.cdc",
    "publication.name": "moa_changelog_pub",
    "slot.name": "moa_changelog_slot",
    "plugin.name": "pgoutput",
    "schema.include.list": "moa",
    "table.include.list": "moa.graph_changelog",
    "snapshot.mode": "no_data",
    "tombstones.on.delete": "false"
  }
}
```

A `moa_replicator` Postgres role is created in the migration with `LOGIN REPLICATION` and SELECT on the changelog.

## 6 Deliverables

- `migrations/M06_graph_changelog.sql` (~250 lines).
- `crates/moa-memory-graph/src/changelog.rs` (~120 lines).
- `ops/debezium/moa-changelog-connector.json`.
- `docs/ops/wal-logical-replication.md` runbook.

## 7 Acceptance criteria

1. Migration + container restart applies cleanly; `SHOW wal_level` returns `logical`.
2. Insert 5 changelog rows; `workspace_state.changelog_version` increments to 5 for that workspace.
3. `UPDATE moa.graph_changelog SET ...` is rejected for `moa_app` (privilege denied).
4. Debezium connector deployed locally consumes 5 rows and emits 5 Kafka messages on `moa.cdc.moa.graph_changelog`.
5. Cross-tenant SELECT on changelog returns 0 rows; auditor SELECT returns all rows.

## 8 Tests

```sh
docker compose down -v && docker compose up -d
cargo run --bin migrate
cargo test -p moa-memory-graph changelog_outbox
docker compose logs debezium | grep "snapshot completed"
```

## 9 Cleanup

- **Delete any pre-existing audit/log tables** from the wiki era (`moa.wiki_log`, `moa.event_audit`, etc.).
- **Remove any code paths that wrote to `_log.md`** filesystem files. Mark with `#[deprecated]` for clean removal in M28.

## 10 What's next

**M07 — `moa-memory-graph` crate scaffold (GraphStore trait + AGE adapter + Cypher templates)**.
