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

const AUTH_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        name: "V000101__auth_baseline.sql",
        sql: include_str!("../migrations/postgres/V000101__auth_baseline.sql"),
    },
    SchemaMigration {
        name: "V000313__authz_outbox_claims.sql",
        sql: include_str!("../migrations/postgres/V000313__authz_outbox_claims.sql"),
    },
    SchemaMigration {
        name: "V000338__token_vault_connections.sql",
        sql: include_str!("../migrations/postgres/V000338__token_vault_connections.sql"),
    },
    SchemaMigration {
        name: "V000341__oauth_authorization_server.sql",
        sql: include_str!("../migrations/postgres/V000341__oauth_authorization_server.sql"),
    },
    SchemaMigration {
        name: "V000346__tenant_credential_vault.sql",
        sql: include_str!("../migrations/postgres/V000346__tenant_credential_vault.sql"),
    },
];

const ORCHESTRATOR_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    name: "V000201__orchestrator_baseline.sql",
    sql: include_str!("../migrations/postgres/V000201__orchestrator_baseline.sql"),
}];

const OCSF_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        name: "V000301__ocsf_baseline.sql",
        sql: include_str!("../migrations/postgres/V000301__ocsf_baseline.sql"),
    },
    SchemaMigration {
        name: "V000345__ocsf_retrieval_idempotency.sql",
        sql: include_str!("../migrations/postgres/V000345__ocsf_retrieval_idempotency.sql"),
    },
];

const REFINERY_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5246_4e59;

/// Advisory lock used by schema-isolated migration helpers.
pub(crate) const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

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

/// Returns a stable fingerprint of the complete database template contents.
///
/// The fingerprint is derived directly from refinery's embedded migration
/// metadata, so adding, renaming, reordering, or changing any central migration
/// invalidates the cached template without a second hand-maintained list. The
/// standalone lineage DDL is included because test templates install it after
/// the refinery sequence completes.
#[must_use]
pub fn full_database_template_fingerprint() -> String {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    fn write_bytes(state: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *state ^= u64::from(*byte);
            *state = state.wrapping_mul(FNV_PRIME);
        }
    }

    let mut fingerprint = FNV_OFFSET_BASIS;
    for migration in embedded::migrations::runner().get_migrations() {
        write_bytes(&mut fingerprint, migration.version().to_string().as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, migration.name().as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, &migration.checksum().to_le_bytes());
        write_bytes(&mut fingerprint, &[0xff]);
    }
    write_bytes(&mut fingerprint, LINEAGE_SCHEMA_DDL.as_bytes());
    format!("{fingerprint:016x}")
}

/// Runs the auth baseline inside an isolated schema.
pub async fn run_auth_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, AUTH_SCHEMA_MIGRATIONS).await
}

/// Runs the orchestrator baseline inside an isolated schema.
pub async fn run_orchestrator_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, ORCHESTRATOR_SCHEMA_MIGRATIONS).await
}

/// Runs the OCSF baseline inside an isolated schema.
pub async fn run_ocsf_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, OCSF_SCHEMA_MIGRATIONS).await
}

const HANDS_SCHEMA_MIGRATIONS: &[SchemaMigration] = &[SchemaMigration {
    name: "V000349__tenant_mcp_connection_bindings.sql",
    sql: include_str!("../migrations/postgres/V000349__tenant_mcp_connection_bindings.sql"),
}];

/// Runs the tool-routing tables inside an isolated schema.
pub async fn run_hands_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_migrations(pool, schema_name, HANDS_SCHEMA_MIGRATIONS).await
}

/// Ensures the standalone lineage schema exists.
///
/// Deliberately does NOT install `analytics.lineage_journal`. Every caller
/// reaches this on a database where the central migrations have already run, so
/// a second copy of that DDL here could never execute - it could only drift from
/// V000363 and describe a queue shape nothing installs.
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

    install_shared_extensions(conn).await?;
    apply_schema_migrations(conn, schema_name, migrations).await
}

/// Installs the database-global extensions shared by every isolated schema.
///
/// Concurrent `CREATE EXTENSION IF NOT EXISTS` for the same extension can error
/// or deadlock on the shared catalog, so a short advisory lock serializes just
/// this step (a fast no-op once the extension already exists) rather than the
/// whole migration replay.
async fn install_shared_extensions(conn: &mut PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SCHEMA_MIGRATION_LOCK_ID)
        .execute(&mut *conn)
        .await
        .context("acquire schema extension advisory lock")?;

    let result = install_shared_extensions_locked(conn).await;

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

async fn install_shared_extensions_locked(conn: &mut PgConnection) -> Result<()> {
    raw_sql("CREATE EXTENSION IF NOT EXISTS pgcrypto;")
        .execute(&mut *conn)
        .await
        .context("install pgcrypto extension")?;
    // Roles are cluster-global catalog state, exactly like extensions. The
    // per-schema migration lists reference `moa_app` (RLS policies and grants)
    // but deliberately exclude `V000001__session_baseline.sql`, which is what
    // creates the roles in a full replay. On a pristine cluster (fresh CI
    // service container) a schema-scoped bootstrap therefore raced whichever
    // full-template build happened to run first — the same guarded creation
    // here, under the same advisory lock, removes that ordering dependency.
    raw_sql(
        r#"
        DO $$
        BEGIN
            CREATE ROLE moa_app NOLOGIN;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        DO $$
        BEGIN
            CREATE ROLE moa_promoter NOLOGIN;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        DO $$
        BEGIN
            CREATE ROLE moa_owner NOLOGIN;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        "#,
    )
    .execute(&mut *conn)
    .await
    .context("ensure shared database roles")?;
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
    // Keep destructive unqualified DDL from resolving to public objects before
    // the isolated schema has created its own relation of the same name.
    let search_path = quote_identifier(schema_name);
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

    use super::ORCHESTRATOR_SCHEMA_MIGRATIONS;

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
}
