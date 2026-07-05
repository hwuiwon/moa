//! DB-backed coverage for workspace authorization outbox tuples.

use anyhow::Result;
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzTupleRow {
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
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
        SELECT tuple_user, tuple_relation, tuple_object, tenant_id
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
            tuple_user: workspace,
            tuple_relation: "workspace".to_string(),
            tuple_object: tenant,
            tenant_id,
        }]
    );

    Ok(())
}
