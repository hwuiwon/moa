//! Clean-apply and idempotency coverage for the central refinery migration runner.
//!
//! These run against a throwaway database created on the configured Postgres
//! instance, so the assertions are independent of any checksum/version drift in
//! the shared central schema. Requires a superuser-capable `MOA_DATABASE_URL`
//! (the local dev `moa_owner`) able to `CREATE DATABASE` and `CREATE EXTENSION`.

use sqlx::Executor;
use sqlx::postgres::PgPoolOptions;

/// Default Docker Compose Postgres URL used by local MOA tests.
const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Returns the Postgres URL used by integration tests, mirroring the runtime
/// `MOA_DATABASE_URL` setting and falling back to the compose default.
fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Returns a process-and-time-unique throwaway database name.
fn unique_db_name() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("moa_mig_idem_{}_{nanos}", std::process::id())
}

/// Rewrites the database name in a Postgres URL, preserving any query string.
fn with_database(url: &str, database: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let prefix = base.rsplit_once('/').map_or(base, |(prefix, _)| prefix);
    match query {
        Some(query) => format!("{prefix}/{database}?{query}"),
        None => format!("{prefix}/{database}"),
    }
}

/// Installs the bootstrap extensions docker initdb would provide, then runs the
/// central migrations twice, returning the applied-migration labels from each run.
async fn clean_apply_then_reapply(
    target_url: &str,
) -> Result<(Vec<String>, Vec<String>), Box<dyn std::error::Error + Send + Sync>> {
    {
        let target = PgPoolOptions::new()
            .max_connections(1)
            .connect(target_url)
            .await?;
        target
            .execute(
                "CREATE EXTENSION IF NOT EXISTS vector; \
                 CREATE EXTENSION IF NOT EXISTS pgaudit;",
            )
            .await?;
        target.close().await;
    }

    let first = moa_migrations::run_reporting_applied(target_url).await?;
    let second = moa_migrations::run_reporting_applied(target_url).await?;
    Ok((first, second))
}

#[tokio::test]
#[ignore = "requires a superuser-capable local Postgres via MOA_DATABASE_URL"]
async fn refinery_clean_apply_then_second_apply_reports_no_new_migrations_db() {
    // Pins clean-apply + idempotency of the central migration runner on a pristine
    // database: the first run applies the full embedded set, and a second run reports
    // zero newly applied migrations. Refinery's schema-history bookkeeping is what
    // makes the re-run a no-op; a migration rewritten to re-run unconditionally, or a
    // non-clean-appliable migration set, would fail one of these assertions.
    let admin_url = test_database_url();
    let db_name = unique_db_name();

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect maintenance database");
    admin
        .execute(format!("CREATE DATABASE \"{db_name}\"").as_str())
        .await
        .expect("create throwaway migration database");

    let target_url = with_database(&admin_url, &db_name);
    let outcome = clean_apply_then_reapply(&target_url).await;

    // Always force-drop the throwaway database, even if an assertion below fails.
    let _ = admin
        .execute(format!("DROP DATABASE IF EXISTS \"{db_name}\" WITH (FORCE)").as_str())
        .await;
    admin.close().await;

    let (first, second) =
        outcome.expect("central migration runs should complete on a fresh database");
    assert!(
        !first.is_empty(),
        "a pristine database must apply migrations on the first run"
    );
    assert!(
        second.is_empty(),
        "second apply must report no newly applied migrations, got {second:?}"
    );
}
