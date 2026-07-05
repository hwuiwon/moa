//! Shared Postgres test helpers for MOA crates.
//!
//! Isolated test databases are provisioned by cloning a single, cached
//! *template* database (`CREATE DATABASE ... TEMPLATE`) rather than replaying the
//! full migration set into a fresh schema for every test. The template is built
//! once per cluster (guarded across processes by an advisory lock and within a
//! process by a [`OnceCell`]) and keyed by a fingerprint of the session
//! migrations so it rebuilds automatically when those migrations change. Each
//! test then gets its own physical database cloned from the template — a fast
//! block copy that preserves full isolation and identical RLS semantics — and
//! drops that database on cleanup.

use std::time::Duration;

use moa_core::{MoaError, Result};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::PostgresSessionStore;

const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Schema name that holds the session tables inside every cloned test database.
///
/// Each test owns its own physical database, so a fixed schema name is safe and
/// keeps the value returned by `schema_name()` stable for callers that qualify
/// table references with it.
const TEMPLATE_SCHEMA: &str = "moa_test";

/// Bumped whenever the template build recipe in [`build_template_contents`]
/// changes (extra schemas, role grants) without a corresponding migration-SQL
/// change. Folded into the template name so cached templates rebuild.
const TEMPLATE_RECIPE_VERSION: u32 = 3;

/// Advisory lock that serializes one-time template-database construction across
/// parallel test processes.
const TEMPLATE_BUILD_LOCK_ID: i64 = 0x4d4f_415f_5450_4c54;

/// Per-process guard so the template is built or validated at most once.
static TEMPLATE: OnceCell<String> = OnceCell::const_new();

/// Returns the Postgres URL used by Postgres-backed tests.
///
/// This is the maintenance database (`moa` by default); `CREATE DATABASE` and
/// `DROP DATABASE` are issued against it, and per-test database URLs are derived
/// from it.
#[must_use]
pub fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Creates a Postgres-backed session store in an isolated database for tests.
///
/// Returns the store, the URL of the per-test database it is connected to, and
/// the schema name that holds the session tables. The database is a clone of the
/// cached migration template, so this performs no migration replay.
pub async fn create_isolated_test_store() -> Result<(PostgresSessionStore, String, String)> {
    let (database_url, schema_name) = provision_cloned_database().await?;
    let store = PostgresSessionStore::new_in_existing_schema(&database_url, &schema_name).await?;
    Ok((store, database_url, schema_name))
}

/// Provisions a fresh per-test database cloned from the migration template.
///
/// Returns the per-test database URL and the schema name holding session tables.
pub async fn provision_cloned_database() -> Result<(String, String)> {
    let template_name = ensure_template().await?;
    let maintenance_url = test_database_url();
    let db_name = format!("moa_test_{}", Uuid::now_v7().simple());
    let mut admin = connect_maintenance(&maintenance_url).await?;

    let statement = format!(
        "CREATE DATABASE {} TEMPLATE {}",
        quote_identifier(&db_name),
        quote_identifier(&template_name)
    );
    let mut attempt = 0_u32;
    let result = loop {
        match sqlx::query(&statement).execute(&mut admin).await {
            Ok(_) => break Ok(()),
            Err(error) if attempt < 4 && is_template_busy(&error) => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100 * u64::from(attempt))).await;
            }
            Err(error) => break Err(error),
        }
    };
    result.map_err(|error| {
        MoaError::StorageError(format!(
            "clone test database {db_name} from template: {error}"
        ))
    })?;

    let url = with_database(&maintenance_url, &db_name)?;
    Ok((url, TEMPLATE_SCHEMA.to_string()))
}

/// Drops the isolated per-test database backing `database_url`.
///
/// `schema_name` is accepted for backwards compatibility; with database-per-test
/// isolation the whole database is dropped. The maintenance database and any
/// shared template database are never dropped.
pub async fn cleanup_test_schema(database_url: &str, _schema_name: &str) -> Result<()> {
    let (_, db_name, _) = split_database_url(database_url)?;
    let maintenance_url = test_database_url();
    let (_, maintenance_db, _) = split_database_url(&maintenance_url)?;

    if db_name == maintenance_db || db_name == "postgres" || db_name.contains("template") {
        return Ok(());
    }

    let mut admin = connect_maintenance(&maintenance_url).await?;
    let statement = format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote_identifier(&db_name)
    );
    let result = sqlx::query(&statement).execute(&mut admin).await;
    result
        .map(|_| ())
        .map_err(|error| MoaError::StorageError(format!("drop test database {db_name}: {error}")))
}

/// Minimum age before a leftover clone database is considered orphaned.
///
/// Clone provisioning has a short window between `CREATE DATABASE` returning
/// and the owning test opening its first connection; during that window the
/// database has no active connection and would otherwise look orphaned to a
/// concurrently starting test process. Clone names embed a UUIDv7 timestamp,
/// so the sweep only drops clones old enough that no live test can own them.
const ORPHAN_CLONE_MIN_AGE: Duration = Duration::from_secs(3600);

/// Returns whether `db_name` is a clone database old enough to sweep.
///
/// Parses the UUIDv7 embedded in `moa_test_<uuid>` names and requires its
/// timestamp to be at least [`ORPHAN_CLONE_MIN_AGE`] in the past. Names that do
/// not carry a parseable UUIDv7 timestamp are never treated as orphaned.
fn clone_name_is_sweepable(db_name: &str, now_unix_secs: u64) -> bool {
    let Some(raw) = db_name.strip_prefix("moa_test_") else {
        return false;
    };
    let Ok(uuid) = Uuid::try_parse(raw) else {
        return false;
    };
    let Some(timestamp) = uuid.get_timestamp() else {
        return false;
    };
    let (created_secs, _) = timestamp.to_unix();
    now_unix_secs.saturating_sub(created_secs) >= ORPHAN_CLONE_MIN_AGE.as_secs()
}

/// Drops leftover per-test clone databases that have no active connections.
///
/// Intended for disk reclamation between test runs. It never drops the shared
/// template, the maintenance database, any database that still has a live
/// connection, or any clone younger than [`ORPHAN_CLONE_MIN_AGE`] (which would
/// race a concurrent test that created its database but has not connected yet).
pub async fn drop_orphaned_test_databases() -> Result<u64> {
    let maintenance_url = test_database_url();
    let (_, maintenance_db, _) = split_database_url(&maintenance_url)?;
    let mut admin = connect_maintenance(&maintenance_url).await?;

    let candidates: Vec<String> = sqlx::query_scalar(
        "SELECT d.datname FROM pg_database d
         WHERE d.datname LIKE 'moa_test_%'
           AND d.datname NOT LIKE 'moa_test_template_%'
           AND d.datname <> $1
           AND NOT EXISTS (
               SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname
           )",
    )
    .bind(&maintenance_db)
    .fetch_all(&mut admin)
    .await
    .map_err(|error| MoaError::StorageError(format!("list orphaned test databases: {error}")))?;

    let now_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let mut dropped = 0_u64;
    for name in candidates {
        if !clone_name_is_sweepable(&name, now_unix_secs) {
            continue;
        }
        let statement = format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_identifier(&name)
        );
        if sqlx::query(&statement).execute(&mut admin).await.is_ok() {
            dropped += 1;
        }
    }
    Ok(dropped)
}

/// Drops cached template databases other than `keep` that have no live
/// connection, reclaiming templates left by older recipe or migration versions.
///
/// Runs under the build advisory lock on `conn`. In-progress staging databases
/// (`*_building_*`) and any template with an open connection are preserved.
async fn drop_stale_templates(conn: &mut sqlx::PgConnection, keep: &str) {
    let stale: Vec<String> = match sqlx::query_scalar(
        "SELECT d.datname FROM pg_database d
         WHERE d.datname LIKE 'moa_test_template_%'
           AND d.datname NOT LIKE '%\\_building\\_%'
           AND d.datname <> $1
           AND NOT EXISTS (
               SELECT 1 FROM pg_stat_activity a WHERE a.datname = d.datname
           )",
    )
    .bind(keep)
    .fetch_all(&mut *conn)
    .await
    {
        Ok(names) => names,
        Err(error) => {
            tracing::warn!(?error, "failed to list stale template databases");
            return;
        }
    };
    for name in stale {
        let statement = format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_identifier(&name)
        );
        if let Err(error) = sqlx::query(&statement).execute(&mut *conn).await {
            tracing::warn!(?error, template = %name, "failed to drop stale template database");
        }
    }
}

/// Returns the cached template database name, building it if necessary.
async fn ensure_template() -> Result<String> {
    let name = TEMPLATE.get_or_try_init(build_template).await?;
    Ok(name.clone())
}

/// Builds (or validates) the migration template database, returning its name.
async fn build_template() -> Result<String> {
    // Best-effort, once per process: reclaim clone databases leaked by prior
    // runs (e.g. tests that panicked before cleanup) so disk does not fill. Only
    // clones with no live connection are dropped, so in-flight tests are safe.
    match drop_orphaned_test_databases().await {
        Ok(dropped) if dropped > 0 => {
            tracing::info!(dropped, "reclaimed orphaned test clone databases");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(?error, "failed to sweep orphaned test databases"),
    }

    let fingerprint = moa_migrations::session_schema_fingerprint();
    let template_name = format!("moa_test_template_{fingerprint}_r{TEMPLATE_RECIPE_VERSION}");
    let maintenance_url = test_database_url();
    let mut conn = connect_maintenance(&maintenance_url).await?;
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEMPLATE_BUILD_LOCK_ID)
        .execute(&mut conn)
        .await
        .map_err(|error| MoaError::StorageError(format!("acquire template build lock: {error}")))?;

    let result = build_template_locked(&mut conn, &maintenance_url, &template_name).await;

    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(TEMPLATE_BUILD_LOCK_ID)
        .execute(&mut conn)
        .await;

    result.map(|()| template_name)
}

/// Builds the template under the held advisory lock.
///
/// Migrations are replayed into a uniquely named staging database that is only
/// renamed to the final template name after the build fully succeeds, so the
/// final name is an atomic readiness signal that survives a crashed builder.
async fn build_template_locked(
    conn: &mut sqlx::PgConnection,
    maintenance_url: &str,
    template_name: &str,
) -> Result<()> {
    // Drop templates from older recipe/migration versions so stale templates do
    // not accumulate across migration changes. Done under the build lock so it
    // never races a concurrent build; staging databases and the current template
    // are preserved.
    drop_stale_templates(conn, template_name).await;

    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(template_name)
            .fetch_one(&mut *conn)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("check template existence: {error}"))
            })?;
    if exists {
        return Ok(());
    }

    let staging = format!("{template_name}_building_{}", std::process::id());
    exec_admin(
        conn,
        &format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_identifier(&staging)
        ),
    )
    .await?;
    exec_admin(
        conn,
        &format!("CREATE DATABASE {}", quote_identifier(&staging)),
    )
    .await?;

    let build_result = build_staging_schema(maintenance_url, &staging).await;
    if let Err(error) = build_result {
        // Best-effort cleanup so a failed build does not leak the staging db.
        let _ = exec_admin(
            conn,
            &format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_identifier(&staging)
            ),
        )
        .await;
        return Err(error);
    }

    exec_admin(
        conn,
        &format!(
            "ALTER DATABASE {} RENAME TO {}",
            quote_identifier(&staging),
            quote_identifier(template_name)
        ),
    )
    .await
}

/// Connects to the staging database and materializes the full template content,
/// closing the build pool before returning so the staging database can be
/// renamed.
async fn build_staging_schema(maintenance_url: &str, staging: &str) -> Result<()> {
    let staging_url = with_database(maintenance_url, staging)?;
    let build_pool = PgPoolOptions::new()
        .min_connections(1)
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(60))
        .connect(&staging_url)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("connect to template staging database: {error}"))
        })?;
    let result = build_template_contents(&build_pool).await;
    build_pool.close().await;
    result
}

/// Materializes the full schema surface every cloned test database needs.
///
/// Runs the session baseline, the OCSF security-event schema (the edge audit
/// path writes `security_events` on every authenticated request), the standalone
/// lineage/analytics schema, and the `moa_app` role grants the runtime expects,
/// so a clone behaves like the shared development database the old
/// schema-per-test harness ran against.
async fn build_template_contents(pool: &PgPool) -> Result<()> {
    moa_migrations::run_session_schema(pool, TEMPLATE_SCHEMA)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("build template session schema: {error:#}"))
        })?;
    moa_migrations::run_ocsf_schema(pool, TEMPLATE_SCHEMA)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("build template ocsf schema: {error:#}"))
        })?;
    moa_migrations::ensure_lineage_schema(pool)
        .await
        .map_err(|error| {
            MoaError::StorageError(format!("build template lineage schema: {error:#}"))
        })?;
    apply_template_grants(pool, TEMPLATE_SCHEMA).await
}

/// Grants the `moa_app` test role the privileges the runtime expects on the
/// template's schemas.
///
/// `run_session_schema` only grants `moa_app` table privileges on RLS-managed
/// tables; it never grants schema `USAGE` on the session or `analytics` schemas,
/// nor `SELECT` on the analytics views the edge analytics store reads through a
/// `moa_app`-scoped connection. The old schema-per-test harness inherited those
/// grants from the shared development database; clones are isolated, so the
/// template must reproduce them. Row visibility is still governed by RLS.
async fn apply_template_grants(pool: &PgPool, schema: &str) -> Result<()> {
    let schema_ident = quote_identifier(schema);
    let schema_literal = quote_literal(schema);
    let statements = [
        format!("GRANT USAGE ON SCHEMA {schema_ident} TO moa_app"),
        "GRANT USAGE ON SCHEMA analytics TO moa_app".to_string(),
        "GRANT SELECT ON ALL TABLES IN SCHEMA analytics TO moa_app".to_string(),
        // Grant SELECT on the session schema's analytics views and materialized
        // views (the analytics store reads these as moa_app). Base-table access
        // is already granted per-table by the session migration's RLS setup.
        format!(
            "DO $$ DECLARE r record; BEGIN \
             FOR r IN SELECT c.relname FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {schema_literal} AND c.relkind IN ('v', 'm') \
             LOOP EXECUTE format('GRANT SELECT ON %I.%I TO moa_app', {schema_literal}, r.relname); \
             END LOOP; END $$"
        ),
    ];
    for statement in statements {
        sqlx::query(&statement)
            .execute(pool)
            .await
            .map_err(|error| {
                MoaError::StorageError(format!("apply template grant failed: {error}"))
            })?;
    }
    Ok(())
}

/// Executes one administrative statement on the maintenance connection.
async fn exec_admin(conn: &mut sqlx::PgConnection, statement: &str) -> Result<()> {
    sqlx::query(statement)
        .execute(&mut *conn)
        .await
        .map(|_| ())
        .map_err(|error| MoaError::StorageError(format!("admin statement failed: {error}")))
}

/// Connects one admin session to the maintenance database for administrative statements.
async fn connect_maintenance(url: &str) -> Result<PgConnection> {
    match tokio::time::timeout(Duration::from_secs(30), PgConnection::connect(url)).await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(error)) => Err(maintenance_connection_error(url, error)),
        Err(_) => Err(maintenance_connection_error(
            url,
            "timed out after 30 seconds",
        )),
    }
}

fn maintenance_connection_error(url: &str, detail: impl std::fmt::Display) -> MoaError {
    MoaError::StorageError(format!(
        "connect to maintenance database failed: {detail}. {}",
        maintenance_connection_hint(url)
    ))
}

fn maintenance_connection_hint(url: &str) -> &'static str {
    if url.contains("127.0.0.1:10040") || url.contains("localhost:10040") {
        "local compose Postgres appears unavailable; start it with `docker compose up -d postgres` and verify `nc -z 127.0.0.1 10040`, or set MOA_DATABASE_URL to another reachable maintenance database"
    } else {
        "check that MOA_DATABASE_URL points to a reachable maintenance database"
    }
}

/// Returns whether a `CREATE DATABASE ... TEMPLATE` error is the transient
/// "template is being accessed by other users" condition worth retrying.
fn is_template_busy(error: &sqlx::Error) -> bool {
    error.to_string().contains("being accessed by other users")
}

/// Splits a Postgres URL into `(prefix, database, suffix)` where `prefix`
/// includes the trailing `/`, `database` is the database name, and `suffix`
/// holds any trailing query string (including the leading `?`).
fn split_database_url(url: &str) -> Result<(String, String, String)> {
    let scheme_end = url
        .find("://")
        .map(|index| index + 3)
        .ok_or_else(|| MoaError::StorageError(format!("database url missing scheme: {url}")))?;
    let authority_and_path = &url[scheme_end..];
    let slash = authority_and_path.find('/').ok_or_else(|| {
        MoaError::StorageError(format!("database url missing database path: {url}"))
    })?;
    let prefix = &url[..scheme_end + slash + 1];
    let rest = &url[scheme_end + slash + 1..];
    let (database, suffix) = match rest.find(['?', '#']) {
        Some(marker) => (&rest[..marker], &rest[marker..]),
        None => (rest, ""),
    };
    Ok((prefix.to_string(), database.to_string(), suffix.to_string()))
}

/// Returns `url` rewritten to point at database `database`, preserving any
/// trailing query string.
fn with_database(url: &str, database: &str) -> Result<String> {
    let (prefix, _, suffix) = split_database_url(url)?;
    Ok(format!("{prefix}{database}{suffix}"))
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_DATABASE_URL, ORPHAN_CLONE_MIN_AGE, clone_name_is_sweepable,
        maintenance_connection_error, split_database_url, with_database,
    };
    use moa_core::MoaError;
    use uuid::{NoContext, Timestamp, Uuid};

    #[test]
    fn orphan_sweep_skips_recent_clone_names() {
        // Pins: a clone created moments ago must never be swept — dropping it
        // races the owning test between CREATE DATABASE and its first connect.
        let name = format!("moa_test_{}", Uuid::now_v7().simple());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs();
        assert!(!clone_name_is_sweepable(&name, now));
    }

    #[test]
    fn orphan_sweep_drops_stale_clone_names_and_ignores_unparseable() {
        // Pins: only clones older than the safety window are reclaimed, and
        // names without a UUIDv7 timestamp are always preserved.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs();
        let stale_secs = now - ORPHAN_CLONE_MIN_AGE.as_secs() - 60;
        let stale_uuid = Uuid::new_v7(Timestamp::from_unix(NoContext, stale_secs, 0));
        let stale_name = format!("moa_test_{}", stale_uuid.simple());
        assert!(clone_name_is_sweepable(&stale_name, now));
        assert!(!clone_name_is_sweepable("moa_test_not_a_uuid", now));
        assert!(!clone_name_is_sweepable("unrelated_db", now));
    }

    #[test]
    fn split_database_url_extracts_database_name() {
        let (prefix, database, suffix) =
            split_database_url("postgres://moa_owner:dev@127.0.0.1:10040/moa").expect("split");
        assert_eq!(prefix, "postgres://moa_owner:dev@127.0.0.1:10040/");
        assert_eq!(database, "moa");
        assert_eq!(suffix, "");
    }

    #[test]
    fn split_database_url_preserves_query_suffix() {
        let (prefix, database, suffix) =
            split_database_url("postgres://u:p@h:5432/db?sslmode=require").expect("split");
        assert_eq!(prefix, "postgres://u:p@h:5432/");
        assert_eq!(database, "db");
        assert_eq!(suffix, "?sslmode=require");
    }

    #[test]
    fn with_database_rewrites_only_the_database_segment() {
        let rewritten = with_database("postgres://u:p@h:5432/moa?sslmode=require", "moa_test_abc")
            .expect("rewrite");
        assert_eq!(
            rewritten,
            "postgres://u:p@h:5432/moa_test_abc?sslmode=require"
        );
    }

    #[test]
    fn maintenance_connect_error_points_to_compose_postgres_for_default_url() {
        // Pins: local setup failures should tell developers how to start and
        // verify the compose Postgres dependency instead of surfacing only a
        // generic SQLx connection error.
        let message = match maintenance_connection_error(DEFAULT_DATABASE_URL, "connection refused")
        {
            MoaError::StorageError(message) => message,
            error => panic!("unexpected error variant: {error:?}"),
        };
        assert!(message.contains("docker compose up -d postgres"));
        assert!(message.contains("nc -z 127.0.0.1 10040"));
    }

    #[test]
    fn maintenance_connect_error_uses_database_url_hint_for_custom_url() {
        // Pins: custom maintenance databases should point at the override knob
        // without leaking or assuming the repository compose endpoint.
        let message = match maintenance_connection_error(
            "postgres://moa_owner:secret@db.internal:5432/moa",
            "connection refused",
        ) {
            MoaError::StorageError(message) => message,
            error => panic!("unexpected error variant: {error:?}"),
        };
        assert!(message.contains("MOA_DATABASE_URL"));
        assert!(!message.contains("docker compose up -d postgres"));
    }
}
