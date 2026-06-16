//! Tests for tenant signing-key verification.

use moa_ocsf::signing;
use serde_json::json;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn sign_verify_roundtrip_detects_tamper_and_wrong_key(pool: sqlx::PgPool) {
    // Pins: HMAC verification accepts the original payload and rejects tamper/wrong-key checks.
    let tenant_id = Uuid::from_u128(0x0a);
    let other_tenant_id = Uuid::from_u128(0x0b);
    signing::ensure_key(&pool, tenant_id)
        .await
        .expect("tenant signing key");
    let wrong_key_id = signing::ensure_key(&pool, other_tenant_id)
        .await
        .expect("other tenant signing key");

    let event = json!({
        "actor": { "user": { "uid": "user:00000000-0000-0000-0000-000000000001" } },
        "class_uid": 3002,
        "time": "2026-05-11T00:00:00Z"
    });

    let (signing_key_id, signature_hex, event_jcs) = signing::sign(&pool, tenant_id, &event)
        .await
        .expect("sign event");

    let valid = signing::verify(&pool, signing_key_id, &event_jcs, &signature_hex)
        .await
        .expect("verify original");
    assert!(valid);

    let mut tampered = event_jcs.clone();
    let last = tampered.last_mut().expect("non-empty canonical payload");
    *last = b' ';
    let tamper_valid = signing::verify(&pool, signing_key_id, &tampered, &signature_hex)
        .await
        .expect("verify tampered");
    assert!(!tamper_valid);

    let wrong_key_valid = signing::verify(&pool, wrong_key_id, &event_jcs, &signature_hex)
        .await
        .expect("verify wrong key");
    assert!(!wrong_key_valid);
}
