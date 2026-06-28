//! Behavioral coverage for the txn-local row-level-security GUCs that
//! `ScopedConn` installs. These pin the crate's sole responsibility: every scoped
//! transaction must set `moa.tenant_id`/`search_path` for the duration of the
//! transaction and never leak that scope to a later transaction on the same
//! pooled connection. Without this, RLS policies downstream silently fail open.
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

/// Reads a session GUC by name, returning `None` when it is unset.
async fn read_guc(conn: &mut sqlx::PgConnection, name: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT current_setting($1, true)")
        .bind(name)
        .fetch_one(conn)
        .await
        .expect("read GUC")
}

#[tokio::test]
#[ignore = "requires local Postgres configured through MOA_DATABASE_URL"]
async fn scoped_conn_installs_tenant_gucs_that_are_transaction_local_db() {
    // Pins: begin_tenant sets moa.tenant_id, moa.storage_partition_id and the AGE
    // search_path for the scoped transaction, marks it as not control-plane, and the
    // scope is gone on the next transaction over the same pooled connection.
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
        let search_path = read_guc(scoped.as_mut(), "search_path")
            .await
            .unwrap_or_default();
        assert!(
            search_path.contains("ag_catalog"),
            "search_path must include ag_catalog for AGE, got {search_path:?}"
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
