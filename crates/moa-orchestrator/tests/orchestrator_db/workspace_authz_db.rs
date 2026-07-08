//! DB-backed coverage for workspace authorization outbox tuples.

use anyhow::Result;
use moa_authz::enqueue_raw;
use moa_authz_schema::{MODEL_VERSION, TupleOp};
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzTupleRow {
    idempotency_key: String,
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    model_version: i32,
    tenant_id: Uuid,
}

#[tokio::test]
async fn workspace_tenant_tuple_is_idempotent_db() -> Result<()> {
    // Pins: workspace-admin inheritance depends on exactly one outbox tuple
    // linking workspace:<id> to tenant:<id>.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let workspace_id = moa_core::WORKSPACE_ID;
    let tenant_id = Uuid::new_v4();
    let workspace = format!("workspace:{workspace_id}");
    let tenant = format!("tenant:{tenant_id}");

    enqueue_raw(
        pool,
        TupleOp::Write,
        &workspace,
        "workspace",
        &tenant,
        Some(tenant_id),
    )
    .await?;
    enqueue_raw(
        pool,
        TupleOp::Write,
        &workspace,
        "workspace",
        &tenant,
        Some(tenant_id),
    )
    .await?;

    let rows: Vec<AuthzTupleRow> = sqlx::query_as(
        r#"
        SELECT idempotency_key, op, tuple_user, tuple_relation, tuple_object,
               model_version, tenant_id
        FROM authz_outbox
        WHERE tuple_user = $1
          AND tuple_relation = 'workspace'
          AND tuple_object = $2
        ORDER BY created_at
        "#,
    )
    .bind(&workspace)
    .bind(&tenant)
    .fetch_all(pool)
    .await?;

    assert_eq!(
        rows,
        vec![AuthzTupleRow {
            idempotency_key: format!("write-{tenant}-workspace-{workspace}-v{MODEL_VERSION}"),
            op: "write".to_string(),
            tuple_user: workspace,
            tuple_relation: "workspace".to_string(),
            tuple_object: tenant,
            model_version: MODEL_VERSION as i32,
            tenant_id,
        }]
    );

    Ok(())
}

#[test]
fn workspace_authz_backfill_migration_uses_current_model_version_static() {
    // Pins: the workspace backfill migration is edited in place with the current
    // authz model version so the pre-prod backfill cannot keep writing stale v3 rows.
    let sql = include_str!(
        "../../../moa-migrations/migrations/postgres/V000322__workspace_authz_backfill.sql"
    );

    assert!(
        sql.contains(&format!(
            "'write-tenant:%s-workspace-workspace:%s-v{MODEL_VERSION}'"
        )),
        "V000322 idempotency key suffix must match moa_authz_schema::MODEL_VERSION"
    );
    assert!(
        sql.contains(&format!("\n        {MODEL_VERSION},\n")),
        "V000322 inserted model_version must match moa_authz_schema::MODEL_VERSION"
    );
    assert!(
        !sql.contains("-v3"),
        "V000322 must not retain stale v3 idempotency suffixes"
    );
    assert!(
        !sql.contains("\n        3,\n"),
        "V000322 must not retain stale model_version 3 literals"
    );
}
