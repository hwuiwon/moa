//! Integration coverage for the `moa.graph_changelog` outbox.

use moa_core::TenantId;
use moa_db::ScopedConn;
use moa_memory_graph::{ChangelogRecord, write_and_bump};
use moa_memory_types::ScopeContext;
use moa_session::testing;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(workspace_id: impl AsRef<str>) -> ScopeContext {
    let workspace_id = workspace_id.as_ref();
    let tenant_id = Uuid::parse_str(workspace_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(workspace_id)));
    ScopeContext::tenant(tenant_id)
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.as_bytes().iter().copied().enumerate() {
        let slot = index % 16;
        bytes[slot] = bytes[slot]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(index as u8);
        let mirror = (index * 7 + 3) % 16;
        bytes[mirror] ^= byte.rotate_left((index % 8) as u32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

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

async fn set_tenant_gucs(conn: &mut sqlx::PgConnection, workspace_id: &str) {
    let tenant_id = Uuid::parse_str(workspace_id).expect("test workspace id is a UUID");
    sqlx::query(
        r#"
        SELECT
            pg_catalog.set_config('moa.tenant_id', $1, true),
            pg_catalog.set_config('moa.contact_id', '', true),
            pg_catalog.set_config('moa.control_plane', 'false', true)
        "#,
    )
    .bind(tenant_id.to_string())
    .execute(conn)
    .await
    .expect("set tenant GUCs");
}

fn record(workspace_id: &str, uid: Uuid, index: usize) -> ChangelogRecord {
    ChangelogRecord {
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "tenant".to_string(),
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
    let workspace_a = Uuid::now_v7().to_string();
    let workspace_b = Uuid::now_v7().to_string();
    let ctx = tenant_scope(workspace_a.clone());
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

    set_tenant_gucs(conn.as_mut(), &workspace_b).await;
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
    let workspace_id = Uuid::now_v7().to_string();
    let ctx = tenant_scope(&workspace_id);
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped changelog transaction");
    set_app_role(conn.as_mut()).await;

    let target_uid = Uuid::now_v7();
    write_and_bump(conn.as_mut(), record(&workspace_id, target_uid, 0))
        .await
        .expect("seed changelog record");
    let visible_before = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1",
    )
    .bind(target_uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("count seeded changelog row");
    assert_eq!(visible_before, 1);

    let update_result =
        sqlx::query("UPDATE moa.graph_changelog SET pii_class = 'pii' WHERE target_uid = $1")
            .bind(target_uid)
            .execute(conn.as_mut())
            .await;
    match update_result {
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("permission denied") || message.contains("row-level security"),
                "{message}"
            );
        }
        Ok(result) => {
            assert_eq!(
                result.rows_affected(),
                0,
                "moa_app must not update graph_changelog rows"
            );
            let pii_class = sqlx::query_scalar::<_, String>(
                "SELECT pii_class FROM moa.graph_changelog WHERE target_uid = $1",
            )
            .bind(target_uid)
            .fetch_one(conn.as_mut())
            .await
            .expect("read unchanged changelog row");
            assert_eq!(pii_class, "none");
        }
    }

    conn.rollback()
        .await
        .expect("rollback changelog update transaction");
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
