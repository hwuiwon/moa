//! DB-backed coverage for hosted eval run status RLS behavior.

use anyhow::Result;
use moa_core::{RlsContext, TenantId};
use moa_db::ScopedConn;
use moa_session::PostgresSessionStore;
use moa_session::testing;
use serde_json::json;
use sqlx::{PgPool, raw_sql};
use uuid::Uuid;

const EVAL_RUN_STATUS_MIGRATION: &str =
    include_str!("../../../moa-migrations/migrations/postgres/V000312__eval_run_status.sql");

#[tokio::test]
async fn eval_run_status_lifecycle_round_trips_under_app_role_force_rls_db() -> Result<()> {
    // Pins: app-role eval status writes and reads work only through tenant-scoped RLS.
    let (store, database_url, schema_name) = create_eval_run_status_test_store().await?;
    let pool = store.pool();
    grant_app_role_analytics_schema_usage(pool).await?;
    assert_eval_run_status_force_rls(pool).await?;

    let tenant_id = TenantId::new();
    let run_id = Uuid::now_v7();
    insert_pending_status_as_tenant(pool, tenant_id, run_id).await?;

    let inserted = status_as_tenant(pool, tenant_id, run_id).await?;
    assert_eq!(inserted.as_deref(), Some("pending"));

    mark_running_as_tenant(pool, tenant_id, run_id).await?;
    let running = status_as_tenant(pool, tenant_id, run_id).await?;
    assert_eq!(running.as_deref(), Some("running"));

    persist_terminal_response_as_tenant(pool, tenant_id, run_id).await?;
    let terminal = terminal_response_suite_as_tenant(pool, tenant_id, run_id).await?;
    assert_eq!(terminal.as_deref(), Some("rls-suite"));

    cleanup_eval_run_status_control_plane(pool, &[run_id]).await?;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

#[tokio::test]
async fn eval_run_status_tenant_b_cannot_read_tenant_a_row_db() -> Result<()> {
    // Pins: tenant B cannot read tenant A eval status rows even when filtering only by run id.
    let (store, database_url, schema_name) = create_eval_run_status_test_store().await?;
    let pool = store.pool();
    grant_app_role_analytics_schema_usage(pool).await?;
    assert_eval_run_status_force_rls(pool).await?;

    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let run_id = Uuid::now_v7();
    insert_pending_status_as_tenant(pool, tenant_a, run_id).await?;

    let visible_to_a = visible_run_count_as_tenant(pool, tenant_a, run_id).await?;
    let visible_to_b = visible_run_count_as_tenant(pool, tenant_b, run_id).await?;

    assert_eq!(visible_to_a, 1);
    assert_eq!(visible_to_b, 0);

    cleanup_eval_run_status_control_plane(pool, &[run_id]).await?;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name).await?;
    Ok(())
}

async fn create_eval_run_status_test_store() -> Result<(PostgresSessionStore, String, String)> {
    let (store, database_url, schema_name) = testing::create_isolated_test_store().await?;
    raw_sql(EVAL_RUN_STATUS_MIGRATION)
        .execute(store.pool())
        .await?;
    Ok((store, database_url, schema_name))
}

async fn grant_app_role_analytics_schema_usage(pool: &PgPool) -> Result<()> {
    sqlx::query("GRANT USAGE ON SCHEMA analytics TO moa_app")
        .execute(pool)
        .await?;
    Ok(())
}

async fn assert_eval_run_status_force_rls(pool: &PgPool) -> Result<()> {
    let force_rls: bool = sqlx::query_scalar(
        "SELECT relforcerowsecurity FROM pg_class WHERE oid = 'analytics.eval_run_status'::regclass",
    )
    .fetch_one(pool)
    .await?;
    assert!(force_rls, "analytics.eval_run_status must FORCE RLS");
    Ok(())
}

async fn insert_pending_status_as_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<()> {
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    sqlx::query(
        r#"
        INSERT INTO analytics.eval_run_status (run_id, tenant_id, status, request)
        VALUES ($1, $2, 'pending', $3)
        "#,
    )
    .bind(run_id)
    .bind(tenant_id.0)
    .bind(json!({
        "tenant_id": tenant_id,
        "suite_document": "[suite]\nname = \"rls-suite\"\n",
        "config_documents": [],
        "evaluators": []
    }))
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(())
}

async fn mark_running_as_tenant(pool: &PgPool, tenant_id: TenantId, run_id: Uuid) -> Result<()> {
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    let result = sqlx::query(
        r#"
        UPDATE analytics.eval_run_status
        SET status = 'running', updated_at = now()
        WHERE run_id = $1
        "#,
    )
    .bind(run_id)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    assert_eq!(result.rows_affected(), 1);
    Ok(())
}

async fn persist_terminal_response_as_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<()> {
    let response = json!({
        "tenant_id": tenant_id,
        "run_id": run_id,
        "status": "completed",
        "suite_name": "rls-suite",
        "exit_code": 0,
        "summary": { "passed": 1 },
        "results": [],
        "error": null
    });
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    let result = sqlx::query(
        r#"
        UPDATE analytics.eval_run_status
        SET status = 'completed',
            response = $2,
            error = NULL,
            updated_at = now()
        WHERE run_id = $1
        "#,
    )
    .bind(run_id)
    .bind(response)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    assert_eq!(result.rows_affected(), 1);
    Ok(())
}

async fn status_as_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<Option<String>> {
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM analytics.eval_run_status WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_optional(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(status)
}

async fn terminal_response_suite_as_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<Option<String>> {
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    let suite_name = sqlx::query_scalar::<_, String>(
        "SELECT response->>'suite_name' FROM analytics.eval_run_status WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_optional(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(suite_name)
}

async fn visible_run_count_as_tenant(
    pool: &PgPool,
    tenant_id: TenantId,
    run_id: Uuid,
) -> Result<i64> {
    let mut conn = app_role_tenant_conn(pool, tenant_id).await?;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM analytics.eval_run_status WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(count)
}

async fn app_role_tenant_conn<'pool>(
    pool: &'pool PgPool,
    tenant_id: TenantId,
) -> Result<ScopedConn<'pool>> {
    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(tenant_id)).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    Ok(conn)
}

async fn cleanup_eval_run_status_control_plane(pool: &PgPool, run_ids: &[Uuid]) -> Result<()> {
    let mut conn = ScopedConn::begin_control_plane(pool).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    sqlx::query("DELETE FROM analytics.eval_run_status WHERE run_id = ANY($1)")
        .bind(run_ids)
        .execute(conn.as_mut())
        .await?;
    conn.commit().await?;
    Ok(())
}
