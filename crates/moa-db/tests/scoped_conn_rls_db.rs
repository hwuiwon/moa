//! Behavioral coverage for the txn-local row-level-security GUCs that
//! `ScopedConn` installs. These pin the crate's sole responsibility: every scoped
//! transaction must set MOA RLS GUCs for the duration of the
//! transaction and never leak that scope to a later transaction on the same
//! pooled connection. The policy-protected fixture table proves the GUCs are
//! consumed by Postgres RLS under the app role.
//!
//! Requires a reachable Postgres (`MOA_DATABASE_URL`, else the compose default);
//! no migrations or schema are needed because custom GUCs are session-scoped.

use moa_core::{RlsContext, TenantId};
use moa_db::ScopedConn;
use sqlx::postgres::PgPoolOptions;

/// Default Docker Compose Postgres URL used by local MOA tests.
const DEFAULT_DATABASE_URL: &str = "postgres://moa_owner:dev@127.0.0.1:10040/moa";

/// Returns the Postgres URL used by integration tests, mirroring the runtime
/// `MOA_DATABASE_URL` setting and falling back to the compose default.
fn test_database_url() -> String {
    std::env::var("MOA_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string())
}

/// Returns a double-quoted PostgreSQL identifier.
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Returns a schema-qualified table name.
fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "{}.{}",
        quote_identifier(schema_name),
        quote_identifier(table_name)
    )
}

/// Reads a session GUC by name, returning `None` when it is unset.
async fn read_guc(conn: &mut sqlx::PgConnection, name: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT current_setting($1, true)")
        .bind(name)
        .fetch_one(conn)
        .await
        .expect("read GUC")
}

async fn create_rls_fixture(pool: &sqlx::PgPool) -> (String, String) {
    let schema_name = format!(
        "scoped_conn_rls_{}",
        TenantId::new().to_string().replace('-', "")
    );
    let quoted_schema = quote_identifier(&schema_name);
    let table = qualified(&schema_name, "tenant_rows");
    sqlx::query(&format!("CREATE SCHEMA {quoted_schema}"))
        .execute(pool)
        .await
        .expect("create RLS fixture schema");
    sqlx::query(&format!(
        "CREATE TABLE {table} (storage_partition_id TEXT NOT NULL, label TEXT NOT NULL)"
    ))
    .execute(pool)
    .await
    .expect("create RLS fixture table");
    for (statement, context) in [
        (
            format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY"),
            "enable row-level security",
        ),
        (
            format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY"),
            "force row-level security",
        ),
        (
            format!(
                "CREATE POLICY tenant_select ON {table} FOR SELECT TO moa_app \
                 USING (storage_partition_id = current_setting('moa.storage_partition_id', true))"
            ),
            "create tenant RLS policy",
        ),
        (
            format!("GRANT USAGE ON SCHEMA {quoted_schema} TO moa_app"),
            "grant schema usage to app role",
        ),
        (
            format!("GRANT SELECT ON {table} TO moa_app"),
            "grant fixture select to app role",
        ),
    ] {
        sqlx::query(&statement)
            .execute(pool)
            .await
            .unwrap_or_else(|error| panic!("{context}: {error}"));
    }
    (schema_name, table)
}

async fn drop_schema(pool: &sqlx::PgPool, schema_name: &str) {
    sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS {} CASCADE",
        quote_identifier(schema_name)
    ))
    .execute(pool)
    .await
    .expect("drop RLS fixture schema");
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn scoped_conn_installs_tenant_gucs_that_are_transaction_local_db() {
    // Pins: begin_tenant sets the MOA tenant/storage GUCs for the scoped
    // transaction, marks it as not control-plane, and the scope is gone on the
    // next transaction over the same pooled connection.
    // A single connection forces the post-commit read onto the same backend, so this
    // proves the GUCs are transaction-local (set_config is_local=true), not leaked.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url())
        .await
        .expect("connect to Postgres");

    let tenant = TenantId::new();
    let expected_partition = RlsContext::tenant(tenant)
        .storage_partition_id()
        .to_string();

    {
        let mut scoped = ScopedConn::begin_tenant(&pool, tenant)
            .await
            .expect("begin tenant-scoped transaction");

        assert_eq!(
            read_guc(scoped.as_mut(), "moa.tenant_id").await.as_deref(),
            Some(tenant.to_string().as_str()),
            "moa.tenant_id must be set to the scoped tenant inside the transaction"
        );
        assert_eq!(
            read_guc(scoped.as_mut(), "moa.storage_partition_id")
                .await
                .as_deref(),
            Some(expected_partition.as_str()),
            "moa.storage_partition_id must match the tenant partition"
        );
        assert_eq!(
            read_guc(scoped.as_mut(), "moa.control_plane")
                .await
                .as_deref(),
            Some("false"),
            "tenant scope must not be flagged as control-plane"
        );
        scoped.commit().await.expect("commit scoped transaction");
    }

    // A fresh transaction on the same (max_connections=1) pooled connection must no
    // longer see any tenant scope: the GUC was transaction-local.
    let mut tx = pool.begin().await.expect("begin plain transaction");
    let leaked = read_guc(&mut tx, "moa.tenant_id").await.unwrap_or_default();
    assert!(
        leaked.is_empty(),
        "moa.tenant_id must be empty after commit, leaked {leaked:?}"
    );
    tx.rollback().await.expect("rollback plain transaction");
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn scoped_conn_control_plane_sets_flag_with_empty_tenant_db() {
    // Pins: begin_control_plane raises moa.control_plane='true' with an empty tenant,
    // distinguishing the control-plane path from tenant scope.
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&test_database_url())
        .await
        .expect("connect to Postgres");

    let mut scoped = ScopedConn::begin_control_plane(&pool)
        .await
        .expect("begin control-plane transaction");

    assert_eq!(
        read_guc(scoped.as_mut(), "moa.control_plane")
            .await
            .as_deref(),
        Some("true"),
        "control-plane transaction must set moa.control_plane=true"
    );
    assert!(
        read_guc(scoped.as_mut(), "moa.tenant_id")
            .await
            .unwrap_or_default()
            .is_empty(),
        "control-plane transaction must leave moa.tenant_id empty"
    );

    scoped
        .commit()
        .await
        .expect("commit control-plane transaction");
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn scoped_conn_tenant_guc_filters_policy_protected_rows_for_app_role_db() {
    // Pins: begin_tenant installs the same storage-partition GUC that production
    // RLS policies consume, so the app role only sees rows for that tenant.
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&test_database_url())
        .await
        .expect("connect to Postgres");
    let (schema_name, table) = create_rls_fixture(&pool).await;

    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let partition_a = RlsContext::tenant(tenant_a)
        .storage_partition_id()
        .to_string();
    let partition_b = RlsContext::tenant(tenant_b)
        .storage_partition_id()
        .to_string();

    sqlx::query(&format!(
        "INSERT INTO {table} (storage_partition_id, label) VALUES ($1, $2), ($3, $4)"
    ))
    .bind(&partition_a)
    .bind("tenant-a-visible")
    .bind(&partition_b)
    .bind("tenant-b-hidden")
    .execute(&pool)
    .await
    .expect("seed RLS fixture rows");

    let mut scoped = ScopedConn::begin_tenant(&pool, tenant_a)
        .await
        .expect("begin tenant-scoped transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(scoped.as_mut())
        .await
        .expect("assume app role");

    let labels: Vec<String> =
        sqlx::query_scalar(&format!("SELECT label FROM {table} ORDER BY label"))
            .fetch_all(scoped.as_mut())
            .await
            .expect("select RLS-filtered fixture rows");
    assert_eq!(
        labels,
        vec!["tenant-a-visible".to_string()],
        "app-role RLS must expose only rows matching the scoped tenant"
    );

    scoped
        .rollback()
        .await
        .expect("rollback scoped transaction");
    drop_schema(&pool, &schema_name).await;
}
