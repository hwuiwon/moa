//! DB-backed API-key management authorization coverage.

use anyhow::{Context, Result, anyhow};
use axum::{Json, Router, extract::State, routing::post};
use moa_authz::{FgaClient, FgaConfig};
use moa_core::{
    TenantId,
    traits::{Identity, IdentityType},
};
use moa_orchestrator::services::api_keys::{
    list_keys_for_identity, revoke_key_for_identity, rotate_key_for_identity,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzTupleRow {
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    tenant_id: Uuid,
}

#[derive(Debug, sqlx::FromRow)]
struct KeyStateRow {
    revoked_reason: Option<String>,
}

#[tokio::test]
async fn api_key_identity_lists_only_presenting_key_db() -> Result<()> {
    // Pins: API-key authentication cannot enumerate sibling keys with the same owner.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let presenting_key_id = Uuid::new_v4();
    let sibling_key_id = Uuid::new_v4();
    insert_operator_key(&pool, presenting_key_id, tenant_id, owner_id, "presenting").await?;
    insert_operator_key(&pool, sibling_key_id, tenant_id, owner_id, "sibling").await?;

    let rows = list_keys_for_identity(
        pool,
        api_key_identity(owner_id, tenant_id, presenting_key_id),
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    let ids = rows.into_iter().map(|row| row.id).collect::<Vec<_>>();
    assert_eq!(ids, vec![presenting_key_id]);
    Ok(())
}

#[tokio::test]
async fn api_key_identity_cannot_rotate_or_revoke_sibling_without_grant_db() -> Result<()> {
    // Pins: owner-id equality is not sufficient when the caller authenticated with an API key.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let presenting_key_id = Uuid::new_v4();
    let rotate_target_id = Uuid::new_v4();
    let revoke_target_id = Uuid::new_v4();
    insert_operator_key(&pool, presenting_key_id, tenant_id, owner_id, "presenting").await?;
    insert_operator_key(
        &pool,
        rotate_target_id,
        tenant_id,
        owner_id,
        "rotate-target",
    )
    .await?;
    insert_operator_key(
        &pool,
        revoke_target_id,
        tenant_id,
        owner_id,
        "revoke-target",
    )
    .await?;
    let (fga, requests) = spawn_fga_mock(false).await?;
    let identity = api_key_identity(owner_id, tenant_id, presenting_key_id);

    rotate_key_for_identity(
        pool.clone(),
        Some(fga.clone()),
        identity.clone(),
        rotate_target_id,
    )
    .await
    .expect_err("API-key sibling rotate without tenant-admin grant must fail");
    revoke_key_for_identity(
        pool.clone(),
        Some(fga),
        identity,
        revoke_target_id,
        "user_requested",
    )
    .await
    .expect_err("API-key sibling revoke without tenant-admin grant must fail");

    assert_active(&pool, rotate_target_id).await?;
    assert_active(&pool, revoke_target_id).await?;
    let check_requests = check_requests(&requests).await;
    assert_eq!(
        check_requests,
        vec![
            check_body(presenting_key_id, tenant_id),
            check_body(presenting_key_id, tenant_id),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn api_key_identity_can_rotate_sibling_with_explicit_tenant_admin_grant_db() -> Result<()> {
    // Pins: explicit tenant-admin grants are evaluated as api_key:<presenting-key>.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let caller_tenant_id = Uuid::new_v4();
    let target_tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let presenting_key_id = Uuid::new_v4();
    let target_key_id = Uuid::new_v4();
    insert_operator_key(
        &pool,
        presenting_key_id,
        target_tenant_id,
        owner_id,
        "presenting",
    )
    .await?;
    insert_operator_key(&pool, target_key_id, target_tenant_id, owner_id, "target").await?;
    let (fga, requests) = spawn_fga_mock(true).await?;

    let rotated = rotate_key_for_identity(
        pool.clone(),
        Some(fga),
        api_key_identity(owner_id, caller_tenant_id, presenting_key_id),
        target_key_id,
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    assert_revoked_with_reason(&pool, target_key_id, "rotation").await?;
    let new_tenant_id: Uuid = sqlx::query_scalar("SELECT tenant_id FROM api_keys WHERE id = $1")
        .bind(rotated.id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        new_tenant_id, target_tenant_id,
        "rotation should use the target row's stored tenant"
    );
    assert_eq!(
        check_requests(&requests).await,
        vec![check_body(presenting_key_id, target_tenant_id)]
    );
    Ok(())
}

#[tokio::test]
async fn operator_session_identity_can_rotate_owned_key_without_fga_db() -> Result<()> {
    // Pins: non-API-key operator sessions keep existing owner-key management behavior.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let key_id = Uuid::new_v4();
    insert_operator_key(&pool, key_id, tenant_id, owner_id, "owned").await?;

    let rotated = rotate_key_for_identity(
        pool.clone(),
        None,
        operator_identity(owner_id, tenant_id),
        key_id,
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    assert_revoked_with_reason(&pool, key_id, "rotation").await?;
    assert_ne!(rotated.id, key_id);
    Ok(())
}

#[tokio::test]
async fn revoke_enqueues_api_key_role_tuple_deletes_db() -> Result<()> {
    // Pins: revocation deletes owner, tenant, and manual admin/operator API-key grants.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let key_id = Uuid::new_v4();
    insert_operator_key(&pool, key_id, tenant_id, owner_id, "revoke").await?;

    revoke_key_for_identity(
        pool.clone(),
        None,
        operator_identity(owner_id, tenant_id),
        key_id,
        "user_requested",
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    assert_eq!(
        delete_rows_for_key(&pool, key_id).await?,
        expected_key_delete_rows(key_id, tenant_id, owner_id)
    );
    Ok(())
}

#[tokio::test]
async fn rotate_deletes_old_role_grants_without_copying_to_replacement_db() -> Result<()> {
    // Pins: rotation removes manual role grants from the old key but does not grant them to the new key.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let old_key_id = Uuid::new_v4();
    insert_operator_key(&pool, old_key_id, tenant_id, owner_id, "rotate").await?;

    let rotated = rotate_key_for_identity(
        pool.clone(),
        None,
        operator_identity(owner_id, tenant_id),
        old_key_id,
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    assert_eq!(
        delete_rows_for_key(&pool, old_key_id).await?,
        expected_key_delete_rows(old_key_id, tenant_id, owner_id)
    );
    assert_eq!(
        role_write_rows_for_key(&pool, rotated.id).await?,
        Vec::<AuthzTupleRow>::new(),
        "manual admin/operator grants must not carry to the rotated key"
    );
    Ok(())
}

async fn insert_operator_key(
    pool: &sqlx::PgPool,
    api_key_id: Uuid,
    tenant_id: Uuid,
    owner_id: Uuid,
    name: &str,
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO api_keys (id, prefix, hash, owner_user_id, tenant_id, name, env)
        VALUES ($1, $2, $3, $4, $5, $6, 'prod')
        "#,
    )
    .bind(api_key_id)
    .bind(format!("test_{}", api_key_id.simple()))
    .bind("argon2id-test-hash")
    .bind(owner_id)
    .bind(tenant_id)
    .bind(name)
    .execute(pool)
    .await?;
    Ok(())
}

async fn delete_rows_for_key(pool: &sqlx::PgPool, api_key_id: Uuid) -> Result<Vec<AuthzTupleRow>> {
    let key_wire = format!("api_key:{api_key_id}");
    authz_rows(
        pool,
        r#"
        SELECT op, tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE op = 'delete'
          AND (tuple_user = $1 OR tuple_object = $1)
        ORDER BY tuple_user, tuple_relation, tuple_object
        "#,
        &key_wire,
    )
    .await
}

async fn role_write_rows_for_key(
    pool: &sqlx::PgPool,
    api_key_id: Uuid,
) -> Result<Vec<AuthzTupleRow>> {
    let key_wire = format!("api_key:{api_key_id}");
    authz_rows(
        pool,
        r#"
        SELECT op, tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE op = 'write'
          AND tuple_user = $1
          AND tuple_relation IN ('admin', 'operator')
        ORDER BY tuple_user, tuple_relation, tuple_object
        "#,
        &key_wire,
    )
    .await
}

async fn authz_rows(
    pool: &sqlx::PgPool,
    query: &str,
    key_wire: &str,
) -> Result<Vec<AuthzTupleRow>> {
    let rows = sqlx::query_as::<_, AuthzTupleRow>(query)
        .bind(key_wire)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

fn expected_key_delete_rows(key_id: Uuid, tenant_id: Uuid, owner_id: Uuid) -> Vec<AuthzTupleRow> {
    vec![
        AuthzTupleRow {
            op: "delete".to_string(),
            tuple_user: format!("api_key:{key_id}"),
            tuple_relation: "admin".to_string(),
            tuple_object: format!("tenant:{tenant_id}"),
            tenant_id,
        },
        AuthzTupleRow {
            op: "delete".to_string(),
            tuple_user: format!("api_key:{key_id}"),
            tuple_relation: "operator".to_string(),
            tuple_object: format!("tenant:{tenant_id}"),
            tenant_id,
        },
        AuthzTupleRow {
            op: "delete".to_string(),
            tuple_user: format!("operator:{owner_id}"),
            tuple_relation: "owner".to_string(),
            tuple_object: format!("api_key:{key_id}"),
            tenant_id,
        },
        AuthzTupleRow {
            op: "delete".to_string(),
            tuple_user: format!("tenant:{tenant_id}"),
            tuple_relation: "tenant".to_string(),
            tuple_object: format!("api_key:{key_id}"),
            tenant_id,
        },
    ]
}

async fn assert_active(pool: &sqlx::PgPool, api_key_id: Uuid) -> Result<()> {
    let revoked_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = $1")
            .bind(api_key_id)
            .fetch_one(pool)
            .await?;
    assert_eq!(revoked_at, None);
    Ok(())
}

async fn assert_revoked_with_reason(
    pool: &sqlx::PgPool,
    api_key_id: Uuid,
    reason: &str,
) -> Result<()> {
    let row: KeyStateRow = sqlx::query_as("SELECT revoked_reason FROM api_keys WHERE id = $1")
        .bind(api_key_id)
        .fetch_one(pool)
        .await?;
    assert_eq!(row.revoked_reason.as_deref(), Some(reason));
    Ok(())
}

fn operator_identity(owner_id: Uuid, tenant_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: owner_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn api_key_identity(owner_id: Uuid, tenant_id: Uuid, api_key_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: owner_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: Some(api_key_id),
        acting_on_behalf_of: None,
    }
}

#[derive(Clone)]
struct FgaMockState {
    check_allowed: bool,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn spawn_fga_mock(check_allowed: bool) -> Result<(FgaClient, Arc<Mutex<Vec<Value>>>)> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = FgaMockState {
        check_allowed,
        requests: Arc::clone(&requests),
    };
    let app = Router::new()
        .route("/stores/store-1/list-objects", post(fga_list_objects))
        .route("/stores/store-1/check", post(fga_check))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind FGA mock")?;
    let address = listener.local_addr().context("read FGA mock address")?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::debug!(%error, "FGA mock server stopped");
        }
    });

    let client = FgaClient::new(FgaConfig {
        url: format!("http://{address}"),
        preshared_key: "test-token".to_string(),
        store_id: "store-1".to_string(),
        model_id: "model-1".to_string(),
        timeout_ms: 5_000,
    })
    .context("build FGA mock client")?;
    Ok((client, requests))
}

async fn fga_list_objects(
    State(state): State<FgaMockState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    state.requests.lock().await.push(body);
    Json(json!({ "objects": [] }))
}

async fn fga_check(State(state): State<FgaMockState>, Json(body): Json<Value>) -> Json<Value> {
    state.requests.lock().await.push(body);
    Json(json!({ "allowed": state.check_allowed }))
}

async fn check_requests(requests: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    requests
        .lock()
        .await
        .iter()
        .filter(|body| body.get("tuple_key").is_some())
        .cloned()
        .collect()
}

fn check_body(api_key_id: Uuid, tenant_id: Uuid) -> Value {
    json!({
        "authorization_model_id": "model-1",
        "tuple_key": {
            "user": format!("api_key:{api_key_id}"),
            "relation": "admin",
            "object": format!("tenant:{tenant_id}"),
        }
    })
}
