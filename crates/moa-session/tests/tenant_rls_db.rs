use moa_core::{
    Event, ModelId, SessionActorRef, SessionMeta, SessionStore, TenantId, ToolCallId, ToolOutput,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use sqlx::{PgPool, Postgres, Transaction, types::Json};
use std::time::Duration;
use uuid::Uuid;

fn tenant_id() -> TenantId {
    TenantId::from(Uuid::now_v7())
}

fn test_session_meta(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity { id: Uuid::now_v7() }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}

fn qualified(schema_name: &str, table_name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema_name.replace('"', "\"\""),
        table_name.replace('"', "\"\"")
    )
}

async fn grant_app_role_schema_usage(test_db: &TestDb) {
    sqlx::query(&format!(
        "GRANT USAGE ON SCHEMA \"{}\" TO moa_app",
        test_db.schema_name().replace('"', "\"\"")
    ))
    .execute(test_db.store().pool())
    .await
    .expect("grant moa_app usage on isolated schema");
}

async fn begin_app_role_tx<'pool>(
    pool: &'pool PgPool,
    schema_name: &str,
    tenant_id: Option<TenantId>,
    contact_id: Option<Uuid>,
    control_plane: bool,
) -> Transaction<'pool, Postgres> {
    let mut tx = pool.begin().await.expect("begin app-role transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *tx)
        .await
        .expect("set app role");
    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, true)")
        .bind(format!("\"{}\", public", schema_name.replace('"', "\"\"")))
        .execute(&mut *tx)
        .await
        .expect("set test schema search path");
    sqlx::query(
        r#"
        SELECT
            pg_catalog.set_config('moa.tenant_id', $1, true),
            pg_catalog.set_config('moa.contact_id', $2, true),
            pg_catalog.set_config('moa.control_plane', $3, true)
        "#,
    )
    .bind(tenant_id.map(|id| id.to_string()).unwrap_or_default())
    .bind(contact_id.map(|id| id.to_string()).unwrap_or_default())
    .bind(if control_plane { "true" } else { "false" })
    .execute(&mut *tx)
    .await
    .expect("set tenant RLS GUCs");
    tx
}

async fn count_as_app_role(
    test_db: &TestDb,
    tenant_id: Option<TenantId>,
    contact_id: Option<Uuid>,
    control_plane: bool,
    sql: &str,
) -> i64 {
    grant_app_role_schema_usage(test_db).await;
    let mut tx = begin_app_role_tx(
        test_db.store().pool(),
        test_db.schema_name(),
        tenant_id,
        contact_id,
        control_plane,
    )
    .await;
    let count = sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(&mut *tx)
        .await
        .expect("count rows as moa_app");
    tx.rollback().await.expect("rollback app-role read");
    count
}

async fn create_session_for_tenant(test_db: &TestDb, tenant_id: TenantId) {
    test_db
        .store()
        .create_session(test_session_meta(tenant_id))
        .await
        .expect("create tenant session");
}

#[tokio::test]
#[ignore]
async fn tenant_rls_blocks_cross_tenant_session_reads_db() {
    // Pins: tenant-scoped app-role reads see only sessions owned by the current tenant.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    let tenant_a = tenant_id();
    let tenant_b = tenant_id();
    create_session_for_tenant(&test_db, tenant_a).await;
    create_session_for_tenant(&test_db, tenant_b).await;

    let sessions = qualified(test_db.schema_name(), "sessions");
    let visible = count_as_app_role(
        &test_db,
        Some(tenant_a),
        None,
        false,
        &format!("SELECT COUNT(*) FROM {sessions}"),
    )
    .await;

    assert_eq!(visible, 1);
}

#[tokio::test]
#[ignore]
async fn tenant_rls_blocks_cross_tenant_event_reads_db() {
    // Pins: tenant-scoped app-role event reads cannot see another tenant's session log.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    let tenant_a = tenant_id();
    let tenant_b = tenant_id();
    let session_a = test_db
        .store()
        .create_session(test_session_meta(tenant_a))
        .await
        .expect("create tenant A session");
    let session_b = test_db
        .store()
        .create_session(test_session_meta(tenant_b))
        .await
        .expect("create tenant B session");

    let tool_id = ToolCallId(Uuid::now_v7());
    test_db
        .store()
        .emit_event(
            session_a,
            Event::ToolResult {
                tool_id,
                provider_tool_use_id: None,
                output: ToolOutput::text("tenant-a", Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
            },
        )
        .await
        .expect("emit tenant A event");
    test_db
        .store()
        .emit_event(
            session_b,
            Event::ToolResult {
                tool_id: ToolCallId(Uuid::now_v7()),
                provider_tool_use_id: None,
                output: ToolOutput::text("tenant-b", Duration::from_millis(1)),
                original_output_tokens: None,
                success: true,
                duration_ms: 1,
            },
        )
        .await
        .expect("emit tenant B event");

    let events = qualified(test_db.schema_name(), "events");
    let visible = count_as_app_role(
        &test_db,
        Some(tenant_a),
        None,
        false,
        &format!("SELECT COUNT(*) FROM {events}"),
    )
    .await;

    assert_eq!(visible, 1);
}

#[tokio::test]
#[ignore]
async fn tenant_rls_blocks_cross_tenant_ingest_dedup_reads_db() {
    // Pins: ingest dedup runtime rows use tenant_id RLS, not old workspace/user GUCs.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    let tenant_a = tenant_id();
    let tenant_b = tenant_id();
    let fact_a = Uuid::now_v7();
    let fact_b = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO moa.ingest_dedup
            (storage_partition_id, tenant_id, session_id, turn_seq, fact_hash, fact_uid)
        VALUES
            ($1, $2, $3, 1, $4, $5),
            ($6, $7, $8, 1, $9, $10)
        "#,
    )
    .bind(tenant_a.to_string())
    .bind(tenant_a.0)
    .bind(Uuid::now_v7())
    .bind(vec![0x31_u8, 0x32, 0x33, 0x34])
    .bind(fact_a)
    .bind(tenant_b.to_string())
    .bind(tenant_b.0)
    .bind(Uuid::now_v7())
    .bind(vec![0x41_u8, 0x42, 0x43, 0x44])
    .bind(fact_b)
    .execute(test_db.store().pool())
    .await
    .expect("seed ingest dedup tenant rows");

    let scoped_query = format!(
        "SELECT COUNT(*) FROM moa.ingest_dedup WHERE fact_uid IN ('{}'::uuid, '{}'::uuid)",
        fact_a, fact_b
    );
    let unset_visible = count_as_app_role(&test_db, None, None, false, &scoped_query).await;
    let tenant_visible =
        count_as_app_role(&test_db, Some(tenant_a), None, false, &scoped_query).await;

    assert_eq!(unset_visible, 0);
    assert_eq!(tenant_visible, 1);
}

#[tokio::test]
#[ignore]
async fn contact_rls_blocks_other_contact_memory_reads_db() {
    // Pins: contact-scoped graph-memory reads see only rows for that contact.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    let tenant = tenant_id();
    let contact_a = Uuid::now_v7();
    let contact_b = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, contact_id, name, pii_class, confidence, properties_summary)
        VALUES
            ($1, 'Fact', $3, $4, $5, 'contact A memory', 'none', 0.9, $7),
            ($2, 'Fact', $3, $4, $6, 'contact B memory', 'none', 0.9, $8)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7())
    .bind(tenant.to_string())
    .bind(tenant.0)
    .bind(contact_a)
    .bind(contact_b)
    .bind(Json(serde_json::json!({"owner": "contact-a"})))
    .bind(Json(serde_json::json!({"owner": "contact-b"})))
    .execute(test_db.store().pool())
    .await
    .expect("seed contact memory rows");

    let visible = count_as_app_role(
        &test_db,
        Some(tenant),
        Some(contact_a),
        false,
        "SELECT COUNT(*) FROM moa.node_index WHERE name LIKE 'contact % memory'",
    )
    .await;

    assert_eq!(visible, 1);
}

#[tokio::test]
#[ignore]
async fn workspace_control_plane_scope_can_read_all_tenants_db() {
    // Pins: explicit control-plane scope can read tenant rows across tenant boundaries.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    create_session_for_tenant(&test_db, tenant_id()).await;
    create_session_for_tenant(&test_db, tenant_id()).await;

    let sessions = qualified(test_db.schema_name(), "sessions");
    let visible = count_as_app_role(
        &test_db,
        None,
        None,
        true,
        &format!("SELECT COUNT(*) FROM {sessions}"),
    )
    .await;

    assert_eq!(visible, 2);
}

#[tokio::test]
#[ignore]
async fn tenant_scope_without_guc_fails_closed_db() {
    // Pins: tenant-owned tables fail closed for moa_app and install no owner bypass policy.
    let test_db = bootstrap_test_db().await.expect("bootstrap test db");
    create_session_for_tenant(&test_db, tenant_id()).await;

    let sessions = qualified(test_db.schema_name(), "sessions");
    let app_visible = count_as_app_role(
        &test_db,
        None,
        None,
        false,
        &format!("SELECT COUNT(*) FROM {sessions}"),
    )
    .await;
    let owner_policy_count = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)
        FROM pg_policies
        WHERE schemaname = $1
          AND tablename = 'sessions'
          AND policyname = 'owner_dev_access'
        "#,
    )
    .bind(test_db.schema_name())
    .fetch_one(test_db.store().pool())
    .await
    .expect("inspect session RLS policies");
    let force_rls = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT relforcerowsecurity
        FROM pg_class
        WHERE oid = $1::regclass
        "#,
    )
    .bind(&sessions)
    .fetch_one(test_db.store().pool())
    .await
    .expect("inspect forced RLS flag");

    assert_eq!(app_visible, 0);
    assert_eq!(owner_policy_count, 0);
    assert!(force_rls, "sessions must force row-level security");
}
