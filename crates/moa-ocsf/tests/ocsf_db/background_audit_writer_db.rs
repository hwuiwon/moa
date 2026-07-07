//! Integration tests for the background batch audit writer and the signing-key
//! cache it relies on.

use std::time::Duration;

use moa_core::{
    TenantId,
    traits::{Identity, IdentityType},
};
use moa_ocsf::{dropped_audit_count, init_background_audit, signing, spawn_authn_success};
use uuid::Uuid;

use super::support;

async fn security_event_count(pool: &sqlx::PgPool, tenant_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM security_events WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .expect("count security events")
}

#[tokio::test]
async fn background_audit_writer_persists_signed_events_off_the_request_path_db() {
    // Pins: spawn_authn_success enqueues events that the background writer signs
    // and batch-inserts, and nothing is dropped when the queue has headroom.
    let pool = support::migrated_ocsf_pool().await;
    init_background_audit(pool.clone());

    let tenant_id = Uuid::from_u128(0x501);
    let user_id = Uuid::from_u128(0x502);
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: user_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    };

    let expected = 5;
    for _ in 0..expected {
        spawn_authn_success(tenant_id, &identity, "api_key", Some("127.0.0.1"));
    }

    // Poll until the background writer has flushed (age flush is ~500ms).
    let mut count = 0;
    for _ in 0..50 {
        count = security_event_count(&pool, tenant_id).await;
        if count >= expected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(count, expected, "all enqueued events are persisted");
    assert_eq!(dropped_audit_count(), 0, "no events dropped with headroom");

    // The background-signed rows verify against the tenant's signing key.
    let (signing_key_id, signature_hex, event_jcs): (Uuid, String, Vec<u8>) = sqlx::query_as(
        "SELECT signing_key_id, signature_hex, event_jcs FROM security_events WHERE tenant_id = $1 LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("fetch a persisted row");
    assert!(
        signing::verify(&pool, signing_key_id, &event_jcs, &signature_hex)
            .await
            .expect("verify signature"),
        "background-written signature verifies"
    );
}

#[tokio::test]
async fn sign_cached_reuses_active_key_and_rotation_invalidates_cache_db() {
    // Pins: the signing-key cache serves the same active key across events and is
    // invalidated on rotation so a rotated key is picked up immediately.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::from_u128(0x777);
    let event = serde_json::json!({
        "class_uid": 3002,
        "activity_id": 1,
        "category_uid": 3,
        "severity_id": 1,
        "type_uid": 300201,
    });

    let (first_key, _, _) = signing::sign_cached(&pool, tenant_id, &event)
        .await
        .expect("first sign creates and caches a key");
    let (second_key, _, _) = signing::sign_cached(&pool, tenant_id, &event)
        .await
        .expect("second sign reuses the cached key");
    assert_eq!(first_key, second_key, "cached active key is reused");

    let rotated = signing::rotate_key(&pool, tenant_id)
        .await
        .expect("rotate key");
    assert_ne!(rotated, first_key, "rotation mints a new key");

    let (post_rotation_key, signature_hex, event_jcs) =
        signing::sign_cached(&pool, tenant_id, &event)
            .await
            .expect("sign after rotation");
    assert_eq!(
        post_rotation_key, rotated,
        "rotation invalidated the cache so the new key is used"
    );
    assert!(
        signing::verify(&pool, post_rotation_key, &event_jcs, &signature_hex)
            .await
            .expect("verify signature"),
        "signature from the rotated key verifies"
    );
}

#[tokio::test]
async fn sign_cached_concurrent_first_use_shares_created_key_db() {
    // Pins: concurrent first audit events for a tenant do not race on active-key
    // creation and drop one signer on the active-key unique index.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::from_u128(0x778);
    let event = serde_json::json!({
        "class_uid": 3002,
        "activity_id": 1,
        "category_uid": 3,
        "severity_id": 1,
        "type_uid": 300201,
    });

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let pool = pool.clone();
        let event = event.clone();
        tasks.spawn(async move {
            signing::sign_cached(&pool, tenant_id, &event)
                .await
                .map(|(key_id, _, _)| key_id)
        });
    }

    let mut keys = Vec::new();
    while let Some(result) = tasks.join_next().await {
        keys.push(
            result
                .expect("signing task should not panic")
                .expect("sign"),
        );
    }

    assert_eq!(keys.len(), 16);
    assert!(
        keys.iter().all(|key| *key == keys[0]),
        "all concurrent signers should use the same active key"
    );
}
