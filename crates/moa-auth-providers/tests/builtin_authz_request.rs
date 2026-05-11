//! Tests for builtin async authorization request storage.

use std::sync::Arc;
use std::time::Duration;

use moa_auth_providers::BuiltinAsyncAuthzProvider;
use moa_core::traits::{ApprovalRequest, AsyncAuthzProvider};
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires DATABASE_URL pointing at a local Postgres test database"]
async fn request_approval_inserts_pending_row(pool: sqlx::PgPool) {
    // Pins: builtin provider persists exactly one pending approval for the awakeable.
    let provider = BuiltinAsyncAuthzProvider::new(Arc::new(pool.clone()));
    let session_id = Uuid::from_u128(10);
    let user_id = Uuid::from_u128(11);
    let tenant_id = Uuid::from_u128(12);
    let awakeable_id = "awakeable_test_123".to_string();

    let handle = provider
        .request_approval(ApprovalRequest {
            session_id,
            deciding_user_id: user_id,
            action_summary: "send email".to_string(),
            action_details: serde_json::json!({
                "_tenant_id": tenant_id.to_string(),
                "to": "alice@example.com",
            }),
            awakeable_id: awakeable_id.clone(),
            timeout: Duration::from_secs(60),
        })
        .await
        .expect("approval request inserts row");

    assert_eq!(handle.awakeable_id, awakeable_id);
    assert_eq!(
        handle.provider_specific,
        serde_json::json!({ "kind": "builtin" })
    );

    let (status, count): (String, i64) = sqlx::query_as(
        r#"
        SELECT status, COUNT(*)
        FROM builtin_pending_approvals
        WHERE session_id = $1 AND deciding_user_id = $2 AND awakeable_id = $3
        GROUP BY status
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(&handle.awakeable_id)
    .fetch_one(&pool)
    .await
    .expect("approval row exists");

    assert_eq!(status, "pending");
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires DATABASE_URL pointing at a local Postgres test database"]
async fn request_approval_requires_tenant_id_in_action_details(pool: sqlx::PgPool) {
    // Pins: missing internal tenant metadata is rejected before any row is inserted.
    let provider = BuiltinAsyncAuthzProvider::new(Arc::new(pool.clone()));
    let error = provider
        .request_approval(ApprovalRequest {
            session_id: Uuid::from_u128(20),
            deciding_user_id: Uuid::from_u128(21),
            action_summary: "send email".to_string(),
            action_details: serde_json::json!({ "to": "alice@example.com" }),
            awakeable_id: "awakeable_missing_tenant".to_string(),
            timeout: Duration::from_secs(60),
        })
        .await
        .expect_err("missing tenant id should fail");

    assert_eq!(
        error.to_string(),
        "internal: request.action_details._tenant_id required"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM builtin_pending_approvals")
        .fetch_one(&pool)
        .await
        .expect("count approval rows");
    assert_eq!(count, 0);
}
