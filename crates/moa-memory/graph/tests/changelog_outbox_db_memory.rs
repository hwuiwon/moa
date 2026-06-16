//! Integration coverage for the `moa.graph_changelog` outbox.

use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{ChangelogRecord, write_and_bump};
use moa_session::testing;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

async fn set_auditor_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("RESET ROLE")
        .execute(&mut *conn)
        .await
        .expect("reset role");
    sqlx::query("SET LOCAL ROLE moa_auditor")
        .execute(conn)
        .await
        .expect("set moa_auditor role");
}

async fn set_workspace_gucs(conn: &mut sqlx::PgConnection, workspace_id: &str) {
    sqlx::query("SELECT pg_catalog.set_config('moa.workspace_id', $1, true)")
        .bind(workspace_id)
        .execute(&mut *conn)
        .await
        .expect("set workspace GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.user_id', '', true)")
        .execute(&mut *conn)
        .await
        .expect("clear user GUC");
    sqlx::query("SELECT pg_catalog.set_config('moa.scope_tier', 'workspace', true)")
        .execute(conn)
        .await
        .expect("set scope tier GUC");
}

fn record(workspace_id: &str, uid: Uuid, index: usize) -> ChangelogRecord {
    ChangelogRecord {
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        actor_id: None,
        actor_kind: "system".to_string(),
        op: "create".to_string(),
        target_kind: "node".to_string(),
        target_label: "Fact".to_string(),
        target_uid: uid,
        payload: json!({ "after": { "index": index } }),
        redaction_marker: None,
        pii_class: "none".to_string(),
        audit_metadata: Some(json!({ "test": "changelog_outbox" })),
        cause_change_id: None,
    }
}

#[tokio::test]
async fn changelog_write_bumps_workspace_version_and_respects_read_rls() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_a = format!("changelog-{}-a", Uuid::now_v7().simple());
    let workspace_b = format!("changelog-{}-b", Uuid::now_v7().simple());
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_a.clone()));
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped changelog transaction");
    set_app_role(conn.as_mut()).await;

    let mut target_uids = Vec::with_capacity(5);
    for index in 0..5 {
        let uid = Uuid::now_v7();
        write_and_bump(conn.as_mut(), record(&workspace_a, uid, index))
            .await
            .expect("write changelog record");
        target_uids.push(uid);
    }

    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(&workspace_a)
    .fetch_one(conn.as_mut())
    .await
    .expect("read workspace changelog version");
    assert_eq!(version, 5);

    let own_visible = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = ANY($1)",
    )
    .bind(target_uids.as_slice())
    .fetch_one(conn.as_mut())
    .await
    .expect("count own changelog rows");
    assert_eq!(own_visible, 5);

    set_workspace_gucs(conn.as_mut(), &workspace_b).await;
    let cross_tenant_visible = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = ANY($1)",
    )
    .bind(target_uids.as_slice())
    .fetch_one(conn.as_mut())
    .await
    .expect("count cross-tenant changelog rows");
    assert_eq!(cross_tenant_visible, 0);

    set_auditor_role(conn.as_mut()).await;
    let auditor_visible = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = ANY($1)",
    )
    .bind(target_uids.as_slice())
    .fetch_one(conn.as_mut())
    .await
    .expect("count auditor changelog rows");
    assert_eq!(auditor_visible, 5);

    conn.rollback()
        .await
        .expect("rollback changelog transaction");
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn changelog_rejects_updates_for_app_role() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("changelog-update-{}", Uuid::now_v7().simple());
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped changelog transaction");
    set_app_role(conn.as_mut()).await;

    let error = sqlx::query("UPDATE moa.graph_changelog SET pii_class = 'none' WHERE false")
        .execute(conn.as_mut())
        .await
        .expect_err("moa_app must not be able to update graph_changelog");
    let message = error.to_string();
    assert!(
        message.contains("permission denied") || message.contains("row-level security"),
        "{message}"
    );

    conn.rollback()
        .await
        .expect("rollback changelog update transaction");
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
