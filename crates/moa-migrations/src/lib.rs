//! Central `PostgreSQL` migrations for MOA.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::{Acquire, PgConnection, PgPool, raw_sql};
use tokio_postgres::{Client, NoTls};

/// Refinery migrations embedded into the migration runner binary.
mod embedded {
    use refinery::embed_migrations;

    embed_migrations!("migrations/postgres");
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MigrationIdentity {
    version: i32,
    name: String,
    checksum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryRow {
    version: Option<String>,
    name: Option<String>,
    checksum: Option<String>,
}

#[derive(Clone, Copy)]
enum HistoryRequirement {
    Prefix,
    Complete,
}

#[derive(Clone, Copy)]
struct SchemaFragment {
    name: &'static str,
    sql: &'static str,
}

const TENANT_CONNECTOR_CONNECTIONS_SQL: &str =
    include_str!("../migrations/postgres/V000050__tenant_connector_connections.sql");
const CONNECTOR_CONNECTION_USE_GRANTS_SQL: &str =
    include_str!("../migrations/postgres/V000051__connector_connection_use_grants.sql");
const CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_BEGIN: &str =
    "-- BEGIN TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT";
const CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_END: &str =
    "-- END TENANT CONNECTOR CREDENTIAL SLOT AUTH FRAGMENT";
const STAGED_CREDENTIAL_OPERATION_FRAGMENT_BEGIN: &str =
    "-- BEGIN STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT";
const STAGED_CREDENTIAL_OPERATION_FRAGMENT_END: &str =
    "-- END STAGED TENANT CREDENTIAL OPERATION AUTH FRAGMENT";

const AUTH_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[
    SchemaFragment {
        name: "auth_baseline",
        sql: include_str!("../migrations/postgres/V000003__auth_baseline.sql"),
    },
    SchemaFragment {
        name: "authz_outbox_claims",
        sql: include_str!("../migrations/postgres/V000013__authz_outbox_claims.sql"),
    },
    SchemaFragment {
        name: "token_vault_connections",
        sql: include_str!("../migrations/postgres/V000029__token_vault_connections.sql"),
    },
    SchemaFragment {
        name: "oauth_authorization_server",
        sql: include_str!("../migrations/postgres/V000032__oauth_authorization_server.sql"),
    },
    SchemaFragment {
        name: "tenant_credential_vault",
        sql: include_str!("../migrations/postgres/V000036__tenant_credential_vault.sql"),
    },
];

fn auth_schema_fragments() -> Result<Vec<SchemaFragment>> {
    let mut fragments = AUTH_SCHEMA_FRAGMENTS.to_vec();
    fragments.push(SchemaFragment {
        name: "tenant_connector_connections",
        sql: extract_marked_schema_fragment(
            TENANT_CONNECTOR_CONNECTIONS_SQL,
            CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_BEGIN,
            CONNECTOR_CREDENTIAL_SLOT_FRAGMENT_END,
        )?,
    });
    fragments.push(SchemaFragment {
        name: "connector_connection_use_grants",
        sql: extract_marked_schema_fragment(
            CONNECTOR_CONNECTION_USE_GRANTS_SQL,
            STAGED_CREDENTIAL_OPERATION_FRAGMENT_BEGIN,
            STAGED_CREDENTIAL_OPERATION_FRAGMENT_END,
        )?,
    });
    Ok(fragments)
}

fn extract_marked_schema_fragment<'a>(
    source: &'a str,
    begin_marker: &str,
    end_marker: &str,
) -> Result<&'a str> {
    let begin_offsets = source
        .match_indices(begin_marker)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    let end_offsets = source
        .match_indices(end_marker)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if begin_offsets.len() != 1 || end_offsets.len() != 1 {
        bail!(
            "schema fragment markers must occur exactly once: begin={}, end={}",
            begin_offsets.len(),
            end_offsets.len()
        );
    }
    let fragment_start = begin_offsets[0] + begin_marker.len();
    let fragment_end = end_offsets[0];
    if fragment_start >= fragment_end {
        bail!("schema fragment end marker must follow its begin marker");
    }
    let fragment = source[fragment_start..fragment_end].trim();
    if fragment.is_empty() {
        bail!("schema fragment between markers must not be empty");
    }
    Ok(fragment)
}

const ORCHESTRATOR_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[SchemaFragment {
    name: "orchestrator_baseline",
    sql: include_str!("../migrations/postgres/V000004__orchestrator_baseline.sql"),
}];

const OCSF_SCHEMA_FRAGMENTS: &[SchemaFragment] = &[SchemaFragment {
    name: "ocsf_baseline",
    sql: include_str!("../migrations/postgres/V000005__ocsf_baseline.sql"),
}];

const REFINERY_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5246_4e59;
const SHARED_CATALOG_RETRY_LIMIT: usize = 5;
const REFINERY_HISTORY_TABLE: &str = "public.refinery_schema_history";
const DESTRUCTIVE_RESET_REQUIRED: &str =
    "the database must be destructively rebuilt or reset for the contiguous migration epoch";

/// Advisory lock used by schema-isolated migration helpers.
pub(crate) const SCHEMA_MIGRATION_LOCK_ID: i64 = 0x4d4f_415f_5343_4845;

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

/// Validates that a database has the exact complete embedded migration history.
///
/// Runtime consumers use this instead of applying migrations themselves. A
/// missing, partial, legacy, or checksum-divergent history fails closed.
pub async fn validate_complete_history(pool: &PgPool) -> Result<()> {
    let history_exists: bool = sqlx::query_scalar(
        "SELECT pg_catalog.to_regclass('public.refinery_schema_history') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("check central migration history table")?;
    if !history_exists {
        bail!("central migration history is missing; {DESTRUCTIVE_RESET_REQUIRED}");
    }

    let rows = sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>)>(
        "SELECT history.version::TEXT, history.name::TEXT, history.checksum::TEXT \
         FROM public.refinery_schema_history AS history ORDER BY history.version",
    )
    .fetch_all(pool)
    .await
    .context("read central migration history")?
    .into_iter()
    .map(|(version, name, checksum)| HistoryRow {
        version,
        name,
        checksum,
    })
    .collect::<Vec<_>>();

    validate_history_rows(
        &rows,
        &expected_migration_identities(),
        HistoryRequirement::Complete,
    )
}

/// Connects to Postgres, takes the refinery advisory lock, validates the
/// migration epoch, runs the embedded migrations, and returns the report.
async fn run_embedded_migrations(database_url: &str) -> Result<refinery::Report> {
    let (mut client, connection) = tokio_postgres::connect(database_url, NoTls)
        .await
        .context("connect to Postgres for refinery migrations")?;
    let connection_task = tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "refinery migration connection task failed");
        }
    });

    let result = match client
        .execute(
            "SELECT pg_advisory_lock($1)",
            &[&REFINERY_MIGRATION_LOCK_ID],
        )
        .await
        .context("acquire refinery migration advisory lock")
    {
        Err(error) => Err(error),
        Ok(_) => {
            let run_result = run_with_shared_catalog_retry(&mut client).await;
            let unlock_result = client
                .execute(
                    "SELECT pg_advisory_unlock($1)",
                    &[&REFINERY_MIGRATION_LOCK_ID],
                )
                .await
                .context("release refinery migration advisory lock");

            match (run_result, unlock_result) {
                (Ok(report), Ok(_)) => Ok(report),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        }
    };

    drop(client);
    let _ = connection_task.await;

    if let Ok(report) = &result {
        tracing::info!(
            applied = report.applied_migrations().len(),
            "refinery migrations complete"
        );
    }
    result
}

async fn run_with_shared_catalog_retry(client: &mut Client) -> Result<refinery::Report> {
    let mut applied_migrations = Vec::new();
    for attempt in 1..=SHARED_CATALOG_RETRY_LIMIT {
        validate_history_before_migration(client).await?;
        let mut runner = central_migration_runner();
        runner.set_migration_table_name(REFINERY_HISTORY_TABLE);
        match runner.run_async(&mut *client).await {
            Ok(report) => {
                applied_migrations.extend(report.applied_migrations().iter().cloned());
                return Ok(refinery::Report::new(applied_migrations));
            }
            Err(error)
                if attempt < SHARED_CATALOG_RETRY_LIMIT
                    && is_shared_catalog_concurrency_error(&error) =>
            {
                if let Some(report) = error.report() {
                    applied_migrations.extend(report.applied_migrations().iter().cloned());
                }
                tracing::warn!(
                    attempt,
                    retry_limit = SHARED_CATALOG_RETRY_LIMIT,
                    "retrying migration after concurrent cluster-role catalog update"
                );
                tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
            }
            Err(error) => return Err(error).context("run refinery migrations"),
        }
    }
    unreachable!("the bounded migration retry loop always returns")
}

fn is_shared_catalog_concurrency_error(error: &refinery::Error) -> bool {
    let refinery::error::Kind::Connection(_, source) = error.kind() else {
        return false;
    };
    source
        .downcast_ref::<tokio_postgres::Error>()
        .and_then(tokio_postgres::Error::as_db_error)
        .is_some_and(|error| error.message() == "tuple concurrently updated")
}

async fn validate_history_before_migration(client: &Client) -> Result<()> {
    let expected = expected_migration_identities();
    validate_expected_migrations(&expected)?;

    let history_exists: bool = client
        .query_one(
            "SELECT pg_catalog.to_regclass('public.refinery_schema_history') IS NOT NULL",
            &[],
        )
        .await
        .context("check central migration history table")?
        .get(0);

    let rows = if history_exists {
        client
            .query(
                "SELECT history.version::TEXT, history.name::TEXT, history.checksum::TEXT \
                 FROM public.refinery_schema_history AS history ORDER BY history.version",
                &[],
            )
            .await
            .context("read central migration history")?
            .into_iter()
            .map(|row| HistoryRow {
                version: row.get(0),
                name: row.get(1),
                checksum: row.get(2),
            })
            .collect()
    } else {
        Vec::new()
    };

    if rows.is_empty() {
        reject_untracked_product_relations(client).await?;
        return Ok(());
    }

    validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
}

async fn reject_untracked_product_relations(client: &Client) -> Result<()> {
    let has_product_relations: bool = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 \
                 FROM pg_catalog.pg_class AS c \
                 JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace \
                 WHERE n.nspname IN ('public', 'moa', 'analytics', 'pii_vault') \
                   AND c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
                   AND NOT (n.nspname = 'public' AND c.relname = 'refinery_schema_history') \
                   AND NOT EXISTS ( \
                       SELECT 1 \
                       FROM pg_catalog.pg_depend AS d \
                       WHERE d.classid = 'pg_catalog.pg_class'::pg_catalog.regclass \
                         AND d.objid = c.oid \
                         AND d.deptype = 'e' \
                   ) \
             )",
            &[],
        )
        .await
        .context("inspect database for untracked product relations")?
        .get(0);

    if has_product_relations {
        bail!(
            "product relations exist without contiguous central migration history; {DESTRUCTIVE_RESET_REQUIRED}"
        );
    }
    Ok(())
}

fn expected_migration_identities() -> Vec<MigrationIdentity> {
    central_migration_runner()
        .get_migrations()
        .iter()
        .map(|migration| MigrationIdentity {
            version: migration.version(),
            name: migration.name().to_string(),
            checksum: migration.checksum(),
        })
        .collect()
}

fn central_migration_runner() -> refinery::Runner {
    let mut migrations = embedded::migrations::runner().get_migrations().clone();
    migrations.sort_by_key(refinery::Migration::version);
    refinery::Runner::new(&migrations)
}

fn validate_expected_migrations(expected: &[MigrationIdentity]) -> Result<()> {
    if expected.is_empty() {
        bail!("the embedded central migration set is empty");
    }
    for (index, migration) in expected.iter().enumerate() {
        let expected_version = i32::try_from(index + 1).context("migration count exceeds i32")?;
        if migration.version != expected_version {
            bail!(
                "embedded central migrations must be exactly contiguous from V000001; expected version {expected_version}, found {}",
                migration.version
            );
        }
    }
    Ok(())
}

fn validate_history_rows(
    rows: &[HistoryRow],
    expected: &[MigrationIdentity],
    requirement: HistoryRequirement,
) -> Result<()> {
    validate_expected_migrations(expected)?;
    if rows.len() > expected.len() {
        bail!(
            "central migration history has {} rows but this build embeds only {}; {DESTRUCTIVE_RESET_REQUIRED}",
            rows.len(),
            expected.len()
        );
    }
    if matches!(requirement, HistoryRequirement::Complete) && rows.len() != expected.len() {
        bail!(
            "central migration history is incomplete: found {} of {} expected rows; {DESTRUCTIVE_RESET_REQUIRED}",
            rows.len(),
            expected.len()
        );
    }

    for (index, (row, expected_row)) in rows.iter().zip(expected).enumerate() {
        let position = index + 1;
        let version = row
            .version
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("central migration history row {position} has a null version; {DESTRUCTIVE_RESET_REQUIRED}"))?
            .parse::<i32>()
            .with_context(|| {
                format!(
                    "central migration history row {position} has a malformed version; {DESTRUCTIVE_RESET_REQUIRED}"
                )
            })?;
        let name = row.name.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "central migration history row {position} has a null name; {DESTRUCTIVE_RESET_REQUIRED}"
            )
        })?;
        let checksum = row
            .checksum
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("central migration history row {position} has a null checksum; {DESTRUCTIVE_RESET_REQUIRED}"))?
            .parse::<u64>()
            .with_context(|| {
                format!(
                    "central migration history row {position} has a malformed checksum; {DESTRUCTIVE_RESET_REQUIRED}"
                )
            })?;

        if version != expected_row.version
            || name != expected_row.name
            || checksum != expected_row.checksum
        {
            bail!(
                "central migration history diverges at row {position}: found V{version:06}__{name} checksum {checksum}, expected V{:06}__{} checksum {}; {DESTRUCTIVE_RESET_REQUIRED}",
                expected_row.version,
                expected_row.name,
                expected_row.checksum
            );
        }
    }
    Ok(())
}

/// Returns a stable fingerprint of the complete database template contents.
///
/// The fingerprint is derived directly from refinery's embedded migration
/// metadata, so adding, renaming, reordering, or changing any central migration
/// invalidates the cached template without a second hand-maintained list.
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
    for migration in expected_migration_identities() {
        write_bytes(&mut fingerprint, migration.version.to_string().as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, migration.name.as_bytes());
        write_bytes(&mut fingerprint, &[0]);
        write_bytes(&mut fingerprint, &migration.checksum.to_le_bytes());
        write_bytes(&mut fingerprint, &[0xff]);
    }
    format!("{fingerprint:016x}")
}

/// Runs the auth DDL fragments inside an isolated schema.
pub async fn run_auth_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    let fragments = auth_schema_fragments()?;
    run_schema_fragments(pool, schema_name, &fragments).await
}

/// Runs the orchestrator DDL fragments inside an isolated schema.
pub async fn run_orchestrator_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_fragments(pool, schema_name, ORCHESTRATOR_SCHEMA_FRAGMENTS).await
}

/// Runs the OCSF DDL fragments inside an isolated schema.
pub async fn run_ocsf_schema(pool: &PgPool, schema_name: &str) -> Result<()> {
    run_schema_fragments(pool, schema_name, OCSF_SCHEMA_FRAGMENTS).await
}

async fn run_schema_fragments(
    pool: &PgPool,
    schema_name: &str,
    fragments: &[SchemaFragment],
) -> Result<()> {
    let mut conn = pool
        .acquire()
        .await
        .context("acquire schema fragment connection")?;
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
    apply_schema_fragments(conn, schema_name, fragments).await
}

/// Installs the database-global extensions shared by every isolated schema.
///
/// Concurrent `CREATE EXTENSION IF NOT EXISTS` for the same extension can error
/// or deadlock on the shared catalog, so a short advisory lock serializes just
/// this step (a fast no-op once the extension already exists) rather than the
/// whole fragment replay.
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
    ensure_shared_database_roles(conn).await
}

async fn ensure_shared_database_roles(conn: &mut PgConnection) -> Result<()> {
    // Roles are cluster-global catalog state, exactly like extensions. The
    // per-schema fragment lists reference `moa_app` (RLS policies and grants)
    // but deliberately exclude `V000002__session_baseline.sql`, which creates
    // the roles in a full replay. On a pristine cluster a schema-scoped
    // bootstrap can run first, so the same guarded creation lives here.
    const ROLE_SQL: &str = r#"
        DO $$
        BEGIN
            CREATE ROLE moa_app NOLOGIN NOBYPASSRLS;
        EXCEPTION
            WHEN duplicate_object THEN NULL;
            WHEN unique_violation THEN NULL;
        END $$;
        DO $$
        BEGIN
            CREATE ROLE moa_promoter NOLOGIN NOBYPASSRLS;
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
        ALTER ROLE moa_app NOLOGIN NOBYPASSRLS;
        ALTER ROLE moa_promoter NOLOGIN NOBYPASSRLS;
        "#;

    for attempt in 1..=SHARED_CATALOG_RETRY_LIMIT {
        match raw_sql(ROLE_SQL).execute(&mut *conn).await {
            Ok(_) => return Ok(()),
            Err(error)
                if attempt < SHARED_CATALOG_RETRY_LIMIT
                    && is_sqlx_shared_catalog_concurrency_error(&error) =>
            {
                tracing::warn!(
                    attempt,
                    retry_limit = SHARED_CATALOG_RETRY_LIMIT,
                    "retrying schema bootstrap after concurrent cluster-role catalog update"
                );
                tokio::time::sleep(Duration::from_millis(25 * attempt as u64)).await;
            }
            Err(error) => return Err(error).context("ensure shared database roles"),
        }
    }
    unreachable!("the bounded schema-role retry loop always returns")
}

fn is_sqlx_shared_catalog_concurrency_error(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(error) if error.message() == "tuple concurrently updated")
}

async fn apply_schema_fragments(
    conn: &mut PgConnection,
    schema_name: &str,
    fragments: &[SchemaFragment],
) -> Result<()> {
    let mut tx = conn
        .begin()
        .await
        .context("begin schema fragment transaction")?;
    // Keep destructive unqualified DDL from resolving to public objects before
    // the isolated schema has created its own relation of the same name.
    let search_path = quote_identifier(schema_name);
    for fragment in fragments {
        sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema fragment search_path")?;
        sqlx::query("SELECT pg_catalog.set_config('moa.migration_search_path', $1, true)")
            .bind(&search_path)
            .execute(&mut *tx)
            .await
            .context("set schema fragment search_path GUC")?;
        raw_sql(fragment.sql)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("run schema fragment {} for {schema_name}", fragment.name))?;
    }

    tx.commit().await.context("commit schema fragments")?;
    Ok(())
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::{
        HistoryRequirement, HistoryRow, MigrationIdentity, OCSF_SCHEMA_FRAGMENTS,
        ORCHESTRATOR_SCHEMA_FRAGMENTS, auth_schema_fragments, central_migration_runner,
        expected_migration_identities, extract_marked_schema_fragment, validate_history_rows,
    };

    fn row(identity: &MigrationIdentity) -> HistoryRow {
        HistoryRow {
            version: Some(identity.version.to_string()),
            name: Some(identity.name.clone()),
            checksum: Some(identity.checksum.to_string()),
        }
    }

    #[test]
    fn actual_runner_orders_migrations_exactly_contiguous_from_one() {
        // Pins: the actual runner passed to Refinery receives one gap-free,
        // numerically ordered V1..N epoch regardless of macro/filesystem order.
        let migrations = central_migration_runner();
        let versions = migrations
            .get_migrations()
            .iter()
            .map(refinery::Migration::version)
            .collect::<Vec<_>>();
        let expected = (1..=i32::try_from(versions.len()).expect("migration count fits i32"))
            .collect::<Vec<_>>();

        assert!(!versions.is_empty(), "expected central migrations");
        assert_eq!(versions, expected);
    }

    #[test]
    fn schema_fragments_are_retained_embedded_migrations_in_order() {
        // Pins: isolated-schema helpers reuse retained final-shape SQL only.
        let embedded_names = expected_migration_identities()
            .into_iter()
            .map(|migration| migration.name)
            .collect::<Vec<_>>();

        let auth_fragments = auth_schema_fragments()
            .expect("the V50 credential-slot fragment markers must be exact");

        for fragments in [
            auth_fragments.as_slice(),
            ORCHESTRATOR_SCHEMA_FRAGMENTS,
            OCSF_SCHEMA_FRAGMENTS,
        ] {
            let positions = fragments
                .iter()
                .map(|fragment| {
                    embedded_names
                        .iter()
                        .position(|name| name == fragment.name)
                        .unwrap_or_else(|| panic!("missing embedded fragment {}", fragment.name))
                })
                .collect::<Vec<_>>();
            assert!(
                positions.windows(2).all(|window| window[0] < window[1]),
                "schema fragments must preserve embedded order"
            );
        }
    }

    #[test]
    fn marked_schema_fragment_requires_one_ordered_nonempty_pair() {
        // Pins: isolated auth bootstrap cannot silently omit or ambiguously select
        // the V50 credential-slot DDL when marker comments drift.
        assert_eq!(
            extract_marked_schema_fragment("before BEGIN\nSELECT 1;\nEND after", "BEGIN", "END")
                .expect("one ordered marker pair should extract"),
            "SELECT 1;"
        );

        for malformed in [
            "SELECT 1; END",
            "BEGIN SELECT 1;",
            "BEGIN SELECT 1; BEGIN SELECT 2; END",
            "BEGIN SELECT 1; END END",
            "END SELECT 1; BEGIN",
            "BEGIN   END",
        ] {
            extract_marked_schema_fragment(malformed, "BEGIN", "END")
                .expect_err("missing, duplicate, reversed, or empty markers must fail closed");
        }
    }

    #[test]
    fn complete_history_requires_every_exact_identity() {
        // Pins: runtime startup fails closed on partial history.
        let expected = expected_migration_identities();
        let rows = expected[..expected.len() - 1]
            .iter()
            .map(row)
            .collect::<Vec<_>>();

        let error = validate_history_rows(&rows, &expected, HistoryRequirement::Complete)
            .expect_err("partial history must fail");
        assert!(error.to_string().contains("history is incomplete"));
        assert!(error.to_string().contains("destructively rebuilt or reset"));
    }

    #[test]
    fn migration_preflight_accepts_an_exact_prefix() {
        // Pins: a clean interrupted rollout may continue from an exact prefix.
        let expected = expected_migration_identities();
        let rows = expected[..3].iter().map(row).collect::<Vec<_>>();

        validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
            .expect("exact prefix is resumable");
    }

    #[test]
    fn migration_preflight_rejects_legacy_epoch_identity() {
        // Pins: the old sparse V1 session baseline cannot be mistaken for the epoch marker.
        let expected = expected_migration_identities();
        let rows = vec![HistoryRow {
            version: Some("1".to_string()),
            name: Some("session_baseline".to_string()),
            checksum: Some("0".to_string()),
        }];

        let error = validate_history_rows(&rows, &expected, HistoryRequirement::Prefix)
            .expect_err("legacy history must fail");
        assert!(error.to_string().contains("diverges at row 1"));
    }

    #[test]
    fn migration_preflight_rejects_malformed_and_divergent_rows() {
        // Pins: corrupt history is diagnosed before refinery can panic or execute DDL.
        let expected = expected_migration_identities();
        let cases = [
            HistoryRow {
                version: Some("not-a-version".to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some(expected[0].checksum.to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: None,
                checksum: Some(expected[0].checksum.to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some("not-a-checksum".to_string()),
            },
            HistoryRow {
                version: Some(expected[0].version.to_string()),
                name: Some(expected[0].name.clone()),
                checksum: Some(expected[0].checksum.wrapping_add(1).to_string()),
            },
        ];

        for corrupt in cases {
            let error = validate_history_rows(&[corrupt], &expected, HistoryRequirement::Prefix)
                .expect_err("corrupt history must fail");
            assert!(error.to_string().contains("destructively rebuilt or reset"));
        }
    }
}
