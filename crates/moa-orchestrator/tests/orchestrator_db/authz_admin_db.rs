//! DB-backed coverage for typed authz administration tuple writes.

use anyhow::Result;
use anyhow::anyhow;
use moa_orchestrator::services::authz_admin::{
    ApiKeyTenantRole, WriteTupleRequest, enqueue_typed_tuple_write,
};
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzTupleRow {
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    tenant_id: Uuid,
}

#[tokio::test]
async fn api_key_tenant_role_grant_enqueues_typed_tuple_db() -> Result<()> {
    // Pins: SCIM can still grant api_key:<id>#admin on tenant:<id> through the typed path.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let api_key_id = Uuid::new_v4();
    insert_api_key(&pool, api_key_id, tenant_id).await?;

    enqueue_typed_tuple_write(
        pool.clone(),
        WriteTupleRequest::GrantApiKeyTenantRole {
            api_key_id,
            tenant_id,
            relation: ApiKeyTenantRole::Admin,
        },
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    let rows = authz_rows(&pool, api_key_id).await?;
    assert_eq!(
        rows,
        vec![AuthzTupleRow {
            op: "write".to_string(),
            tuple_user: format!("api_key:{api_key_id}"),
            tuple_relation: "admin".to_string(),
            tuple_object: format!("tenant:{tenant_id}"),
            tenant_id,
        }]
    );
    Ok(())
}

#[tokio::test]
async fn api_key_tenant_role_uniform_not_found_across_key_states_db() -> Result<()> {
    // Pins (F18): foreign-tenant, nonexistent, and revoked keys all fail with the
    // same not-found result and enqueue nothing, so a caller cannot distinguish
    // key existence or cross-tenant ownership from the tuple-write outcome.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let requested_tenant_id = Uuid::new_v4();

    // Foreign-tenant: the key exists but belongs to a different tenant.
    let foreign_key_id = Uuid::new_v4();
    insert_api_key(&pool, foreign_key_id, Uuid::new_v4()).await?;

    // Nonexistent: no row for this id at all.
    let missing_key_id = Uuid::new_v4();

    // Revoked: the key belongs to the requested tenant but is revoked.
    let revoked_key_id = Uuid::new_v4();
    insert_api_key(&pool, revoked_key_id, requested_tenant_id).await?;
    revoke_api_key(&pool, revoked_key_id).await?;

    let foreign_error = tuple_write_error(&pool, foreign_key_id, requested_tenant_id).await?;
    let missing_error = tuple_write_error(&pool, missing_key_id, requested_tenant_id).await?;
    let revoked_error = tuple_write_error(&pool, revoked_key_id, requested_tenant_id).await?;

    assert!(
        foreign_error.contains("API key not found"),
        "F18 leak: expected uniform not-found, got {foreign_error}"
    );
    assert!(
        !foreign_error.contains("mismatch"),
        "F18 leak: distinguishable tenant-mismatch error, got {foreign_error}"
    );
    assert_eq!(
        foreign_error, missing_error,
        "foreign-tenant and nonexistent keys must be indistinguishable"
    );
    assert_eq!(
        foreign_error, revoked_error,
        "foreign-tenant and revoked keys must be indistinguishable"
    );

    assert_eq!(
        authz_rows(&pool, foreign_key_id).await?,
        Vec::<AuthzTupleRow>::new(),
        "foreign-tenant failure must not enqueue an outbox tuple"
    );
    assert_eq!(
        authz_rows(&pool, revoked_key_id).await?,
        Vec::<AuthzTupleRow>::new(),
        "revoked-key failure must not enqueue an outbox tuple"
    );
    Ok(())
}

async fn tuple_write_error(
    pool: &sqlx::PgPool,
    api_key_id: Uuid,
    tenant_id: Uuid,
) -> Result<String> {
    let error = enqueue_typed_tuple_write(
        pool.clone(),
        WriteTupleRequest::GrantApiKeyTenantRole {
            api_key_id,
            tenant_id,
            relation: ApiKeyTenantRole::Admin,
        },
    )
    .await
    .expect_err("non-owned API-key grants must fail");
    Ok(format!("{error:?}"))
}

async fn insert_api_key(pool: &sqlx::PgPool, api_key_id: Uuid, tenant_id: Uuid) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO api_keys (id, prefix, hash, owner_user_id, tenant_id, name, env)
        VALUES ($1, $2, $3, $4, $5, 'scim', 'prod')
        "#,
    )
    .bind(api_key_id)
    .bind(format!("test_{}", api_key_id.simple()))
    .bind("argon2id-test-hash")
    .bind(Uuid::new_v4())
    .bind(tenant_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn revoke_api_key(pool: &sqlx::PgPool, api_key_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1")
        .bind(api_key_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn authz_rows(pool: &sqlx::PgPool, api_key_id: Uuid) -> Result<Vec<AuthzTupleRow>> {
    let rows = sqlx::query_as(
        r#"
        SELECT op, tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE tuple_user = $1
        ORDER BY created_at
        "#,
    )
    .bind(format!("api_key:{api_key_id}"))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
