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

#[cfg(test)]
const POSTGRES_MIGRATION_FILES: &[&str] = &[
    "V000001__session_baseline.sql",
    "V000101__auth_baseline.sql",
    "V000201__orchestrator_baseline.sql",
    "V000301__ocsf_baseline.sql",
    "V000302__action_policy_auto_mode.sql",
    "V000303__age_rls_operator_resolution.sql",
    "V000304__builtin_approvals_resolved_marker.sql",
    "V000305__retrieval_lineage_turn_id.sql",
    "V000306__contacts.sql",
];

// Schema-isolated session tests do not own artifact/experiment tables. Keep
// this DDL equal to the session-owned prefix of V000302.
const ACTION_POLICY_SCHEMA_MIGRATION_SQL: &str = r#"
DROP TABLE IF EXISTS approval_rules;

CREATE TABLE IF NOT EXISTS action_policy_rules (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    tool TEXT NOT NULL,
    pattern TEXT NOT NULL,
    effect TEXT NOT NULL CHECK (effect IN ('allow', 'deny', 'admin_review')),
    scope TEXT NOT NULL CHECK (scope IN ('global', 'workspace')),
    reason TEXT,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT action_policy_rules_global_workspace_check
        CHECK (
            (scope = 'global' AND workspace_id = 'global')
            OR (scope = 'workspace' AND workspace_id <> 'global')
        )
);

CREATE INDEX IF NOT EXISTS idx_action_policy_rules_scope
    ON action_policy_rules(workspace_id, scope, user_id);
CREATE INDEX IF NOT EXISTS idx_action_policy_rules_lookup
    ON action_policy_rules(workspace_id, tool, user_id, created_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_action_policy_rules_unique_scope
    ON action_policy_rules(workspace_id, tool, pattern, COALESCE(user_id, ''));

SELECT moa.apply_three_tier_rls('action_policy_rules'::REGCLASS);

CREATE TABLE IF NOT EXISTS workspace_action_reviews (
    id UUID PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    user_id TEXT,
    scope TEXT GENERATED ALWAYS AS (moa.compute_scope_tier(workspace_id, user_id)) STORED,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL,
    sub_agent_id TEXT,
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

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_pending
    ON workspace_action_reviews(workspace_id, created_at DESC)
    WHERE status = 'pending';

CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_session
    ON workspace_action_reviews(session_id, created_at DESC)
    WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_workspace_action_reviews_scope
    ON workspace_action_reviews(workspace_id, scope, user_id);

SELECT moa.apply_three_tier_rls('workspace_action_reviews'::REGCLASS);
"#;

const SESSION_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        name: "V000001__session_baseline.sql",
        sql: include_str!("../migrations/postgres/V000001__session_baseline.sql"),
    },
    SchemaMigration {
        name: "V000302__action_policy_auto_mode.sql",
        sql: ACTION_POLICY_SCHEMA_MIGRATION_SQL,
    },
    SchemaMigration {
        name: "V000303__age_rls_operator_resolution.sql",
        sql: include_str!("../migrations/postgres/V000303__age_rls_operator_resolution.sql"),
    },
    SchemaMigration {
        name: "V000305__retrieval_lineage_turn_id.sql",
        sql: include_str!("../migrations/postgres/V000305__retrieval_lineage_turn_id.sql"),
    },
    SchemaMigration {
        name: "V000306__contacts.sql",
        sql: include_str!("../migrations/postgres/V000306__contacts.sql"),
    },
];

const AUTH_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    name: "V000101__auth_baseline.sql",
    sql: include_str!("../migrations/postgres/V000101__auth_baseline.sql"),
}];

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
    Ok(())
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
    let mut lock_conn = pool
        .acquire()
        .await
        .context("acquire migration connection")?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *lock_conn)
        .await
        .context("acquire schema migration advisory lock")?;

    let result = run_schema_migrations_locked(
        &mut lock_conn,
        schema_name,
        migrations,
        install_session_extensions,
    )
    .await;
    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *lock_conn)
        .await
        .context("release schema migration advisory lock");

    match (result, unlock_result) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
    }
}

async fn run_schema_migrations_locked(
    conn: &mut PgConnection,
    schema_name: &str,
    migrations: &[SchemaMigration],
    install_session_extensions: bool,
) -> Result<()> {
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(schema_name)
    ))
    .execute(&mut *conn)
    .await
    .with_context(|| format!("create schema {schema_name}"))?;

    raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto;")
        .execute(&mut *conn)
        .await
        .context("install pgcrypto extension")?;

    if install_session_extensions {
        raw_sql(
            "CREATE EXTENSION IF NOT EXISTS age; \
             LOAD 'age'; \
             CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;",
        )
        .execute(&mut *conn)
        .await
        .context("install session migration extensions")?;
    }

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

    use super::{
        ACTION_POLICY_SCHEMA_MIGRATION_SQL, ORCHESTRATOR_SCHEMA_MIGRATIONS,
        POSTGRES_MIGRATION_FILES,
    };

    #[test]
    fn central_manifest_matches_embedded_postgres_files() {
        let migrations_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
        let mut files = BTreeSet::new();
        for entry in fs::read_dir(&migrations_dir).expect("read central migrations directory") {
            let entry = entry.expect("read central migration entry");
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("sql") {
                let file_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .expect("migration file name is valid UTF-8");
                files.insert(file_name.to_string());
            }
        }

        let manifest = POSTGRES_MIGRATION_FILES
            .iter()
            .map(|name| name.to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(manifest, files);
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
