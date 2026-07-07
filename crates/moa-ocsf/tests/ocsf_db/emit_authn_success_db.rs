//! Integration tests for OCSF authentication event emission.

use moa_core::{
    TenantId,
    traits::{Identity, IdentityType},
};
use moa_ocsf::{emit_authn_success, signing};
use uuid::Uuid;

use super::support;

#[tokio::test]
async fn emit_authn_success_inserts_signed_security_event() {
    // Pins: authn success produces the expected OCSF row and a verifiable signature.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::from_u128(0x10);
    let user_id = Uuid::from_u128(0x20);
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: user_id,
        tenant_id: TenantId::from(tenant_id),
        api_key_id: None,
        acting_on_behalf_of: None,
    };

    let event_id = emit_authn_success(&pool, tenant_id, &identity, "api_key", Some("127.0.0.1"))
        .await
        .expect("emit authn success");

    let row: (i32, i32, i32, String, String, Uuid, Vec<u8>) = sqlx::query_as(
        r#"
        SELECT class_uid, activity_id, severity_id, actor_user_uid, signature_hex,
               signing_key_id, event_jcs
        FROM security_events
        WHERE id = $1
        "#,
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("security event row");

    assert_eq!(row.0, 3002);
    assert_eq!(row.1, 1);
    assert_eq!(row.2, 1);
    assert_eq!(row.3, format!("operator:{user_id}"));
    assert!(!row.4.is_empty());

    let verified = signing::verify(&pool, row.5, &row.6, &row.4)
        .await
        .expect("verify signature");
    assert!(verified);
}
