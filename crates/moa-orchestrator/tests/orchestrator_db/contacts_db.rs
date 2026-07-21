//! DB-harness coverage for contact token bounds and contact session authz tuples.

use anyhow::{Result, anyhow};
use moa_contacts::Error;
use moa_contacts::domain::{
    low_assurance_scopes, require_contact_agent_allowlist, require_contact_agent_permission,
};
use moa_core::{
    types::agent::AgentSessionSelection, types::contact::ContactId,
    types::contact::ContactTokenClaims, types::contact::ContactVerificationState,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_orchestrator::services::contacts::replace_contact_session_authz_tuples;
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct AuthzTupleRow {
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    tenant_id: Uuid,
}

#[test]
fn contact_token_empty_requested_scopes_rejected_db() {
    // Pins: contact token issuance must not silently expand an empty scope request.
    let error = low_assurance_scopes(&[]).expect_err("empty requested scopes should reject");

    assert_terminal(&error, 400, "contact token requested_scopes is required");
}

#[test]
fn contact_token_empty_agent_allowlist_rejected_db() {
    // Pins: contact token issuance requires a concrete agent allowlist.
    let error =
        require_contact_agent_allowlist(&[]).expect_err("empty agent allowlist should reject");

    assert_terminal(&error, 400, "contact token agent_ids is required");
}

#[test]
fn contact_agent_explicit_allowlist_allows_selected_agent_db() {
    // Pins: explicitly allowed agent ids still pass contact session admission.
    let installation_uid = Uuid::new_v4();
    let claims = contact_claims(vec![installation_uid.to_string()]);
    let selection = AgentSessionSelection {
        installation_uid: Some(installation_uid),
        revision_uid: None,
    };

    require_contact_agent_permission(&claims, &selection)
        .expect("explicit agent allowlist should permit the selected agent");
}

#[tokio::test]
async fn contact_session_promotion_replaces_owner_and_contact_tuples_db() -> Result<()> {
    // Pins: promotion replaces both contact-owned session tuple relations.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let old_contact_id = ContactId::new();
    let new_contact_id = ContactId::new();

    replace_contact_session_authz_tuples(
        &pool,
        tenant_id,
        session_id,
        Some(old_contact_id),
        new_contact_id,
    )
    .await
    .map_err(|error| anyhow!("{error:?}"))?;

    let rows = authz_rows(&pool, session_id).await?;
    assert_eq!(
        rows,
        vec![
            AuthzTupleRow {
                op: "delete".to_string(),
                tuple_user: format!("contact:{old_contact_id}"),
                tuple_relation: "contact".to_string(),
                tuple_object: format!("session:{session_id}"),
                tenant_id: tenant_id.0,
            },
            AuthzTupleRow {
                op: "delete".to_string(),
                tuple_user: format!("contact:{old_contact_id}"),
                tuple_relation: "owner".to_string(),
                tuple_object: format!("session:{session_id}"),
                tenant_id: tenant_id.0,
            },
            AuthzTupleRow {
                op: "write".to_string(),
                tuple_user: format!("contact:{new_contact_id}"),
                tuple_relation: "contact".to_string(),
                tuple_object: format!("session:{session_id}"),
                tenant_id: tenant_id.0,
            },
            AuthzTupleRow {
                op: "write".to_string(),
                tuple_user: format!("contact:{new_contact_id}"),
                tuple_relation: "owner".to_string(),
                tuple_object: format!("session:{session_id}"),
                tenant_id: tenant_id.0,
            },
        ]
    );
    Ok(())
}

fn assert_terminal(error: &Error, code: u16, needle: &str) {
    assert_eq!(
        error.terminal_code(),
        Some(code),
        "unexpected terminal code: {error:?}"
    );
    assert!(
        format!("{error}").contains(needle),
        "unexpected message {error:?}, wanted substring {needle:?}"
    );
}

fn contact_claims(agent_ids: Vec<String>) -> ContactTokenClaims {
    ContactTokenClaims {
        iss: "test".to_string(),
        aud: "moa-contact".to_string(),
        sub: ContactId::new().to_string(),
        exp: 4_102_444_800,
        iat: 0,
        nbf: 0,
        jti: Uuid::new_v4().to_string(),
        tenant_id: TenantId::new(),
        state: ContactVerificationState::Unverified,
        scopes: vec!["agent:session:create".to_string()],
        permissions: serde_json::Value::Null,
        agent_ids,
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
        linked_contact_ids: Vec::new(),
    }
}

async fn authz_rows(pool: &sqlx::PgPool, session_id: SessionId) -> Result<Vec<AuthzTupleRow>> {
    let rows = sqlx::query_as(
        r#"
        SELECT op, tuple_user, tuple_relation, tuple_object, tenant_id
        FROM authz_outbox
        WHERE tuple_object = $1
        ORDER BY op, tuple_user, tuple_relation
        "#,
    )
    .bind(format!("session:{session_id}"))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
