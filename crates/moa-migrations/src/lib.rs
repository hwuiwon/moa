//! Central `PostgreSQL` migrations for MOA.

use anyhow::{Context, Result};
use sqlx::{Acquire, Executor, PgConnection, PgPool, raw_sql};
use tokio_postgres::NoTls;

mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations/postgres");
}

struct SchemaMigration {
    name: &'static str,
    sql: &'static str,
}

// Schema-isolated session tests do not own artifact/experiment tables. Keep
// this DDL equal to the session-owned prefix of V000302.
const ACTION_POLICY_SCHEMA_MIGRATION_SQL: &str = r#"
DROP TABLE IF EXISTS approval_rules;

CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('tenant')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT action_policy_rules_global_partition_check
        CHECK (
            scope = 'tenant' AND storage_partition_id <> 'global'
        )
);

CREATE INDEX IF NOT EXISTS idx_action_policy_rules_scope
    ON action_policy_rules(storage_partition_id, scope, user_id);
CREATE INDEX IF NOT EXISTS idx_action_policy_rules_lookup
    ON action_policy_rules(storage_partition_id, tool, user_id, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_action_policy_rules_unique_scope
    ON action_policy_rules(storage_partition_id, tool, pattern, COALESCE(user_id, ''));

SELECT moa.apply_three_tier_rls('action_policy_rules'::REGCLASS);

CREATE TABLE IF NOT EXISTS tenant_action_reviews (
    id UUID PRIMARY KEY,
    storage_partition_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(storage_partition_id, user_id)) STORED,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    worker_id TEXT,
    tool_call_id UUID NOT NULL,
    tool_name TEXT NOT NULL,
    action_class TEXT NOT NULL,
    risk_level TEXT NOT NULL,
    input_summary TEXT NOT NULL,
    normalized_input TEXT NOT NULL,
    envelope JSONB NOT NULL,
    preview JSONB NOT NULL,
    tool_request JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'cleared', 'denied')),
    requested_by TEXT NOT NULL,
    requested_event_recorded_at TIMESTAMPTZ,
    decided_by TEXT,
    deny_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    decided_at TIMESTAMPTZ,
    decision_event_recorded_at TIMESTAMPTZ,
    execution_tool_call_id UUID,
    execution_requested_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_pending
    ON tenant_action_reviews(storage_partition_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_session
    ON tenant_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_tenant_action_reviews_scope
    ON tenant_action_reviews(storage_partition_id, scope, user_id);

SELECT moa.apply_three_tier_rls('tenant_action_reviews'::REGCLASS);
"#;

const SESSION_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        name: "V000001__session_baseline.sql",
        sql: include_str!("../migrations/postgres/V000001__session_baseline.sql"),
    },
    SchemaMigration {
        name: "V000101__auth_baseline.sql",
        sql: include_str!("../migrations/postgres/V000101__auth_baseline.sql"),
    },
    SchemaMigration {
        name: "V000302__action_policy_auto_mode.sql",
        sql: ACTION_POLICY_SCHEMA_MIGRATION_SQL,
    },
    SchemaMigration {
        name: "V000304__retrieval_lineage_turn_id.sql",
        sql: include_str!("../migrations/postgres/V000304__retrieval_lineage_turn_id.sql"),
    },
    SchemaMigration {
        name: "V000305__contacts.sql",
        sql: include_str!("../migrations/postgres/V000305__contacts.sql"),
    },
    SchemaMigration {
        name: "V000306__session_channels.sql",
        sql: include_str!("../migrations/postgres/V000306__session_channels.sql"),
    },
    SchemaMigration {
        name: "V000307__tenant_configurable_agents.sql",
        sql: include_str!("../migrations/postgres/V000307__tenant_configurable_agents.sql"),
    },
    SchemaMigration {
        name: "V000308__tenant_runtime_boundaries.sql",
        sql: include_str!("../migrations/postgres/V000308__tenant_runtime_boundaries.sql"),
    },
    SchemaMigration {
        name: "V000309__graph_changelog_append_only.sql",
        sql: include_str!("../migrations/postgres/V000309__graph_changelog_append_only.sql"),
    },
    SchemaMigration {
        name: "V000310__tenant_knowledge_base.sql",
        sql: include_str!("../migrations/postgres/V000310__tenant_knowledge_base.sql"),
    },
    SchemaMigration {
        name: "V000311__knowledge_connection_source_selection.sql",
        sql: include_str!(
            "../migrations/postgres/V000311__knowledge_connection_source_selection.sql"
        ),
    },
    SchemaMigration {
        name: "V000314__authz_outbox_claims.sql",
        sql: include_str!("../migrations/postgres/V000314__authz_outbox_claims.sql"),
    },
    SchemaMigration {
        name: "V000315__session_blobs.sql",
        sql: include_str!("../migrations/postgres/V000315__session_blobs.sql"),
    },
    SchemaMigration {
        name: "V000316__knowledge_sync_active_claims.sql",
        sql: include_str!("../migrations/postgres/V000316__knowledge_sync_active_claims.sql"),
    },
    SchemaMigration {
        name: "V000317__session_attachments.sql",
        sql: include_str!("../migrations/postgres/V000317__session_attachments.sql"),
    },
    SchemaMigration {
        name: "V000318__knowledge_visibility_cache_invalidation.sql",
        sql: include_str!(
            "../migrations/postgres/V000318__knowledge_visibility_cache_invalidation.sql"
        ),
    },
    SchemaMigration {
        name: "V000319__session_event_dedupe.sql",
        sql: include_str!("../migrations/postgres/V000319__session_event_dedupe.sql"),
    },
    SchemaMigration {
        name: "V000321__vector_sync_outbox.sql",
        sql: include_str!("../migrations/postgres/V000321__vector_sync_outbox.sql"),
    },
    SchemaMigration {
        name: "V000326__analytics_query_read_models.sql",
        sql: include_str!("../migrations/postgres/V000326__analytics_query_read_models.sql"),
    },
];

const AUTH_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        name: "V000101__auth_baseline.sql",
        sql: include_str!("../migrations/postgres/V000101__auth_baseline.sql"),
    },
    SchemaMigration {
        name: "V000314__authz_outbox_claims.sql",
        sql: include_str!("../migrations/postgres/V000314__authz_outbox_claims.sql"),
    },
];

const ORCHESTRATOR_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    name: "V000201__orchestrator_baseline.sql",
    sql: include_str!("../migrations/postgres/V000201__orchestrator_baseline.sql"),
}];

const OCSF_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    name: "V000301__ocsf_baseline.sql",
    sql: include_str!("../migrations/postgres/V000301__ocsf_baseline.sql"),
}];

const REFINERY_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5246_4e59;

/// Advisory lock used by schema-isolated migration helpers.
pub const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

/// Idempotent schema DDL for the engineering-tier lineage tables.
pub const LINEAGE_SCHEMA_DDL: &str = include_str!("../sql/lineage_schema.sql");

/// Focused pgaudit DDL used by audit smoke coverage.
pub const PGAUDIT_SCHEMA_DDL: &str = include_str!("../sql/pgaudit.sql");

/// Runs all central refinery migrations.
pub async fn run(database_url: &str) -> Result<()> {
    run_embedded_migrations(database_url)
        .await
        .map(|_report| ())
}

/// Runs all central refinery migrations and returns the labels of the migrations
/// newly applied by this call.
///
/// On a database that is already up to date the returned list is empty, which is
/// the observable signal callers (and idempotency tests) use to confirm a re-run
/// applied nothing.
pub async fn run_reporting_applied(database_url: &str) -> Result<Vec<String>> {
    let report = run_embedded_migrations(database_url).await?;
    Ok(report
        .applied_migrations()
        .iter()
        .map(|migration| migration.to_string())
        .collect())
}

/// Connects to Postgres, takes the refinery advisory lock, runs the embedded
/// migrations, and returns the refinery report.
async fn run_embedded_migrations(database_url: &str) -> Result<refinery::Report> {
    let (mut client, connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .context("connect to Postgres for refinery migrations")?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "refinery migration connection task failed");
        }
    });

    client
        .execute(
            "SELECT pg_advisory_lock($1)",
            &[&REFINERY_MIGRATION_LOCK_ID],
        )
        .await
        .context("acquire refinery migration advisory lock")?;

    let run_result = embedded::migrations::runner()
        .run_async(&mut client)
        .await
        .context("run refinery migrations");
    let unlock_result = client
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&REFINERY_MIGRATION_LOCK_ID],
        )
        .await
        .context("release refinery migration advisory lock");

    let report = match (run_result, unlock_result) {
        (Ok(report), Ok(_)) => report,
        (Err(error), _) => return Err(error),
        (Ok(_), Err(error)) => return Err(error),
    };

    tracing::info!(
        applied = report.applied_migrations().len(),
        "refinery migrations complete"
    );

    drop(client);
    let _ = connection_task.await;
    Ok(report)
}

/// Returns a stable fingerprint of the session schema migration set.
///
/// The test harness keys its cached template database on this value so the
/// template is rebuilt automatically whenever the session migrations change.
/// `std::hash::DefaultHasher` uses fixed keys, so the result is stable across
/// processes built by the same toolchain (a stale fingerprint at worst forces a
/// one-time template rebuild, never an incorrect schema).
#[must_use]
pub fn session_schema_fingerprint() -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // Version tag so behavioral changes to the helper (extensions, schema
    // layout) invalidate previously cached templates even if the SQL is equal.
    "session-template-v3".hash(&mut hasher);
    for migration in SESSION_SCHEMA_MIGRATIONS {
        migration.name.hash(&mut hasher);
        migration.sql.hash(&mut hasher);
    }
    // The test template also materializes the standalone lineage/analytics
    // schema and the OCSF security-event schema (the edge audit path writes
    // `security_events` on every request), so their DDL is part of the template
    // content and must invalidate the cached template when it changes.
    LINEAGE_SCHEMA_DDL.hash(&mut hasher);
    for migration in OCSF_SCHEMA_MIGRATIONS {
        migration.name.hash(&mut hasher);
        migration.sql.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Runs the session baseline inside an isolated schema.
pub async fn run_session_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, SESSION_SCHEMA_MIGRATIONS, true).await
}

/// Runs the auth baseline inside an isolated schema.
pub async fn run_auth_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, AUTH_SCHEMA_MIGRATIONS, false).await
}

/// Runs the orchestrator baseline inside an isolated schema.
pub async fn run_orchestrator_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, ORCHESTRATOR_SCHEMA_MIGRATIONS, false).await
}

/// Runs the OCSF baseline inside an isolated schema.
pub async fn run_ocsf_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, OCSF_SCHEMA_MIGRATIONS, false).await
}

/// Ensures the standalone lineage schema exists.
pub async fn ensure_lineage_schema(pool: &PgPool) -> Result<()> {
    pool.execute(LINEAGE_SCHEMA_DDL)
        .await
        .context("ensure lineage schema")?;
    Ok(())
}

async fn run_schema_migrations(
    pool: &PgPool,
    schema_name: &str,
    migrations: &[SchemaMigration],
    install_session_extensions: bool,
) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire migration connection")?;
    let conn: &mut PgConnection = &mut conn;

    // Each bootstrap targets a unique schema name, so creating the schema and
    // replaying its DDL never conflicts with concurrent bootstraps and needs no
    // global lock. Only `CREATE EXTENSION` touches database-global catalog state
    // shared across schemas, so that step alone is serialized below.
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(&mut *conn)
    .await
    .with_context(|| format!("create schema {schema_name}"))?;

    install_shared_extensions(conn, install_session_extensions).await?;
    apply_schema_migrations(conn, schema_name, migrations).await
}

/// Installs the database-global extensions shared by every isolated schema.
///
/// Concurrent `CREATE EXTENSION IF NOT EXISTS` for the same extension can error
/// or deadlock on the shared catalog, so a short advisory lock serializes just
/// this step (a fast no-op once the extension already exists) rather than the
/// whole migration replay.
async fn install_shared_extensions(
    conn: &mut PgConnection,
    install_session_extensions: bool,
) -> Result<()> {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *conn)
        .await
        .context("acquire schema extension advisory lock")?;

    let result = install_shared_extensions_locked(conn, install_session_extensions).await;

    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *conn)
        .await
        .context("release schema extension advisory lock");

    match (result, unlock_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn install_shared_extensions_locked(
    conn: &mut PgConnection,
    install_session_extensions: bool,
) -> Result<()> {
    raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto;")
        .execute(&mut *conn)
        .await
        .context("install pgcrypto extension")?;

    if install_session_extensions {
        raw_sql("CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;")
            .execute(&mut *conn)
            .await
            .context("install session migration extensions")?;
    }

    Ok(())
}

async fn apply_schema_migrations(
    conn: &mut PgConnection,
    schema_name: &str,
    migrations: &[SchemaMigration],
) -> Result<()> {
    let mut tx = conn
        .begin()
        .await
        .context("begin schema migration transaction")?;
    let search_path = format!("{}, public", quote_identifier(schema_name));
    for migration in migrations {
        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema migration search_path")?;
        sqlx::query("SELECT pg_catalog.set_config('moa.migration_search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema migration search_path GUC")?;
        raw_sql(migration.sql)
            .execute(&mut *tx)
            .await
            .with_context(|| {
                format!("run schema migration {} for {schema_name}", migration.name)
            })?;
    }

    tx.commit().await.context("commit schema migrations")?;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    use super::{ACTION_POLICY_SCHEMA_MIGRATION_SQL, ORCHESTRATOR_SCHEMA_MIGRATIONS};

    /// Parses a `V<version>__<name>.sql` migration file stem into its numeric
    /// version and refinery-style name, matching how refinery itself parses it.
    fn parse_migration_stem(stem: &str) -> (i64, String) {
        let (version_part, name) = stem
            .split_once("__")
            .unwrap_or_else(|| panic!("migration stem {stem} must contain `__`"));
        let version = version_part
            .trim_start_matches(['V', 'v'])
            .parse::<i64>()
            .unwrap_or_else(|error| panic!("migration {stem} has non-numeric version: {error}"));
        (version, name.to_string())
    }

    #[test]
    fn embedded_migration_set_matches_on_disk_files_and_versions_increase() {
        // Pins the migrations refinery will actually embed and run (via
        // embed_migrations!) against the on-disk directory, rather than a
        // hand-maintained const. Catches a `.sql` added to disk but not picked up,
        // a removed/renamed file, and out-of-order or duplicate version numbers
        // (the checksum/version-drift class) — none of which a string grep saw.
        let migrations_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
        let on_disk: BTreeSet<(i64, String)> = fs::read_dir(&migrations_dir)
            .expect("read central migrations directory")
            .map(|entry| entry.expect("read central migration entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
            .map(|path| {
                let stem = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .expect("migration file stem is valid UTF-8");
                parse_migration_stem(stem)
            })
            .collect();

        let embedded: Vec<(i64, String)> = super::embedded::migrations::runner()
            .get_migrations()
            .iter()
            .map(|migration| (i64::from(migration.version()), migration.name().to_string()))
            .collect();

        let embedded_set: BTreeSet<(i64, String)> = embedded.iter().cloned().collect();
        assert_eq!(
            embedded_set, on_disk,
            "refinery's embedded migration set must match the on-disk .sql files"
        );

        let mut versions: Vec<i64> = embedded.iter().map(|(version, _)| *version).collect();
        let sorted_unique: Vec<i64> = {
            let mut copy = versions.clone();
            copy.sort_unstable();
            copy.dedup();
            copy
        };
        versions.sort_unstable();
        assert_eq!(
            versions, sorted_unique,
            "embedded migration versions must be unique (no duplicate version numbers)"
        );
        assert!(
            !embedded.is_empty(),
            "expected at least one embedded central migration"
        );
    }

    #[test]
    fn orchestrator_agents_status_constraint_is_schema_local() {
        let sql = ORCHESTRATOR_SCHEMA_MIGRATIONS[0].sql;

        assert!(
            sql.contains("conrelid = 'agents'::regclass"),
            "agents_status_check existence check must be scoped to the current schema's agents table"
        );
        assert!(
            sql.contains("ALTER TABLE agents VALIDATE CONSTRAINT agents_status_check"),
            "agents_status_check should still be validated after being added"
        );
    }

    #[test]
    fn action_policy_schema_migration_matches_refinery_session_subset() {
        let refinery_sql =
            include_str!("../migrations/postgres/V000302__action_policy_auto_mode.sql");
        let (session_subset, _) = refinery_sql
            .split_once("\nALTER TABLE moa.artifact_run")
            .expect("action policy migration should end session-owned DDL before artifact DDL");

        assert_eq!(
            ACTION_POLICY_SCHEMA_MIGRATION_SQL.trim(),
            session_subset.trim(),
            "schema-isolated session helper must stay in sync with the session-owned prefix of V000302"
        );
    }
}
