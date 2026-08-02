//! Integration tests pinning the OCSF wire mapping of every `emit_*` helper.
//!
//! Each emitter hard-codes a `(class_uid, activity_id, severity_id)` triple plus
//! a derived `type_uid`. Only `emit_authn_success` had DB coverage, so the rest
//! were free to drift silently. These tests drive the real emitters against a
//! migrated `security_events` schema and assert the persisted classification,
//! prioritizing the deny-audit (`emit_authz_decision`) and credential-failure
//! (`emit_authn_failure`) paths.

use moa_core::{
    traits::{Identity, IdentityType},
    types::identifiers::TenantId,
};
use moa_ocsf::{
    ActorInput, EmitError, ScimGroupAuditChange, emit_agent_deactivated_tx,
    emit_agent_registered_tx, emit_api_key_created_tx, emit_api_key_revoked_tx,
    emit_approval_decided_tx, emit_authn_failure, emit_authn_success, emit_authz_decision,
    emit_delegation_granted_tx, emit_delegation_revoked_tx, emit_scim_group_changes_tx,
    emit_scim_user_created_tx, emit_scim_user_deleted_tx, emit_scim_user_updated_tx,
    emit_user_deactivated_tx, signing,
};
use sqlx::PgPool;
use uuid::Uuid;

use super::support;

/// OCSF class UIDs persisted by MOA emitters.
const ACCOUNT_CHANGE: i32 = 3001;
const AUTHENTICATION: i32 = 3002;
const AUTHORIZATION: i32 = 3003;
const ENTITY_MANAGEMENT: i32 = 3004;

/// Severity IDs persisted by MOA emitters.
const INFORMATIONAL: i32 = 1;
const LOW: i32 = 2;
const MEDIUM: i32 = 3;

#[derive(sqlx::FromRow)]
struct ScimAuditRow {
    id: Uuid,
    class_uid: i32,
    activity_id: i32,
    signing_key_id: Uuid,
    event_jcs: Vec<u8>,
    signature_hex: String,
    target_resource_uid: Option<String>,
}

/// Drive a `_tx` emitter inside its own committed transaction, mirroring the
/// begin/call/commit that production call sites perform around `_tx` helpers.
macro_rules! emit_committed {
    ($pool:expr, $msg:expr, |$tx:ident| $call:expr) => {{
        let mut $tx = $pool.begin().await.expect("begin tx");
        let id = $call.await.expect($msg);
        $tx.commit().await.expect("commit tx");
        id
    }};
}

fn user_identity(tenant_uuid: Uuid, user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: user_id,
        tenant_id: TenantId::from(tenant_uuid),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

async fn class_activity_severity(pool: &PgPool, id: Uuid) -> (i32, i32, i32) {
    sqlx::query_as("SELECT class_uid, activity_id, severity_id FROM security_events WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("security event row")
}

#[tokio::test]
async fn emit_authz_decision_deny_writes_low_severity_authorization_audit_db() {
    // Pins: the deny-audit emitter records an Authorization (3003) "Other" (99)
    // event at Low severity, targets the denied object, and stays HMAC-verifiable.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_uuid = Uuid::from_u128(0x4001);
    let user_id = Uuid::from_u128(0x4002);
    let identity = user_identity(tenant_uuid, user_id);
    let object_uid = format!("session:{}", Uuid::from_u128(0x4003));

    let event_id = emit_authz_decision(
        &pool,
        TenantId::from(tenant_uuid),
        &identity,
        &object_uid,
        "session",
        "read",
        false,
    )
    .await
    .expect("emit authz deny");

    let row: (i32, i32, i32, i64, Option<String>, Option<String>) = sqlx::query_as(
        r#"
        SELECT class_uid, activity_id, severity_id, type_uid, actor_user_uid, target_resource_uid
        FROM security_events
        WHERE id = $1
        "#,
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("security event row");

    assert_eq!(
        row.0, AUTHORIZATION,
        "deny audit must be Authorization class"
    );
    assert_eq!(row.1, 99, "deny maps to authz activity Other (99)");
    assert_eq!(row.2, LOW, "deny audit must be Low severity");
    assert_eq!(
        row.3,
        i64::from(AUTHORIZATION * 100 + 99),
        "type_uid = class*100+activity"
    );
    assert_eq!(
        row.4.as_deref(),
        Some(format!("operator:{user_id}").as_str())
    );
    assert_eq!(row.5.as_deref(), Some(object_uid.as_str()));

    let signed: (Uuid, Vec<u8>, String) = sqlx::query_as(
        "SELECT signing_key_id, event_jcs, signature_hex FROM security_events WHERE id = $1",
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("signing columns");
    let verified = signing::verify(&pool, signed.0, &signed.1, &signed.2)
        .await
        .expect("verify signature");
    assert!(verified, "deny-audit signature must verify");
}

#[tokio::test]
async fn emit_authn_failure_writes_low_severity_credential_validation_db() {
    // Pins: credential failure records an Authentication (3002) Credential
    // Validation (5) event at Low severity attributed to the supplied actor.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_uuid = Uuid::from_u128(0x5001);
    let actor_uid = format!("user:{}", Uuid::from_u128(0x5002));

    let event_id = emit_authn_failure(
        &pool,
        tenant_uuid,
        Some(&actor_uid),
        "api_key",
        Some("203.0.113.7"),
        "bad credential",
    )
    .await
    .expect("emit authn failure");

    let row: (i32, i32, i32, i64, Option<String>) = sqlx::query_as(
        r#"
        SELECT class_uid, activity_id, severity_id, type_uid, actor_user_uid
        FROM security_events
        WHERE id = $1
        "#,
    )
    .bind(event_id)
    .fetch_one(&pool)
    .await
    .expect("security event row");

    assert_eq!(
        row.0, AUTHENTICATION,
        "failure must be Authentication class"
    );
    assert_eq!(row.1, 5, "failure maps to Credential Validation (5)");
    assert_eq!(row.2, LOW, "credential failure must be Low severity");
    assert_eq!(row.3, i64::from(AUTHENTICATION * 100 + 5));
    assert_eq!(row.4.as_deref(), Some(actor_uid.as_str()));
}

#[tokio::test]
async fn emit_matrix_pins_class_activity_severity_for_every_emitter_db() {
    // Pins: every emit_* helper persists its documented (class_uid, activity_id,
    // severity_id) triple. A drift in any hard-coded constant fails exactly the
    // labeled case below.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_uuid = Uuid::from_u128(0x6000);
    let user_id = Uuid::from_u128(0x6001);
    let agent_id = Uuid::from_u128(0x6002);
    let api_key_id = Uuid::from_u128(0x6003);
    let group_id = Uuid::from_u128(0x6004);
    let approval_id = Uuid::from_u128(0x6005);
    let identity = user_identity(tenant_uuid, user_id);
    let actor = ActorInput::user(user_id);

    let mut cases: Vec<(&str, Uuid, (i32, i32, i32))> = Vec::new();

    // Pool-based emitters.
    let id = emit_authn_success(&pool, tenant_uuid, &identity, "api_key", Some("10.0.0.1"))
        .await
        .expect("authn success");
    cases.push(("authn_success", id, (AUTHENTICATION, 1, INFORMATIONAL)));

    let id = emit_authn_failure(
        &pool,
        tenant_uuid,
        Some("user:x"),
        "api_key",
        Some("10.0.0.2"),
        "bad",
    )
    .await
    .expect("authn failure");
    cases.push(("authn_failure", id, (AUTHENTICATION, 5, LOW)));

    let id = emit_authz_decision(
        &pool,
        TenantId::from(tenant_uuid),
        &identity,
        "session:allow",
        "session",
        "read",
        true,
    )
    .await
    .expect("authz allow");
    cases.push((
        "authz_decision_allow",
        id,
        (AUTHORIZATION, 1, INFORMATIONAL),
    ));

    let id = emit_authz_decision(
        &pool,
        TenantId::from(tenant_uuid),
        &identity,
        "session:deny",
        "session",
        "read",
        false,
    )
    .await
    .expect("authz deny");
    cases.push(("authz_decision_deny", id, (AUTHORIZATION, 99, LOW)));

    let id = emit_committed!(&pool, "api key created", |tx| emit_api_key_created_tx(
        &mut tx,
        tenant_uuid,
        &identity,
        api_key_id
    ));
    cases.push(("api_key_created", id, (ENTITY_MANAGEMENT, 1, INFORMATIONAL)));

    let id = emit_committed!(&pool, "api key revoked", |tx| emit_api_key_revoked_tx(
        &mut tx,
        tenant_uuid,
        actor.clone(),
        api_key_id,
        Some("rotation")
    ));
    cases.push(("api_key_revoked", id, (ENTITY_MANAGEMENT, 4, INFORMATIONAL)));

    let id = emit_committed!(&pool, "agent registered", |tx| emit_agent_registered_tx(
        &mut tx,
        tenant_uuid,
        &identity,
        agent_id
    ));
    cases.push(("agent_registered", id, (ACCOUNT_CHANGE, 1, INFORMATIONAL)));

    let id = emit_committed!(&pool, "agent deactivated", |tx| emit_agent_deactivated_tx(
        &mut tx,
        tenant_uuid,
        &identity,
        agent_id
    ));
    cases.push(("agent_deactivated", id, (ACCOUNT_CHANGE, 3, MEDIUM)));

    let id = emit_committed!(&pool, "user created", |tx| emit_scim_user_created_tx(
        &mut tx,
        tenant_uuid,
        actor.clone(),
        user_id
    ));
    cases.push(("user_created", id, (ACCOUNT_CHANGE, 1, INFORMATIONAL)));

    // Transaction-only emitters share one transaction, then commit once.
    let mut tx = pool.begin().await.expect("begin tx");

    let id = emit_api_key_created_tx(&mut tx, tenant_uuid, &identity, api_key_id)
        .await
        .expect("api key created tx");
    cases.push((
        "api_key_created_tx",
        id,
        (ENTITY_MANAGEMENT, 1, INFORMATIONAL),
    ));

    let id = emit_api_key_revoked_tx(&mut tx, tenant_uuid, actor.clone(), api_key_id, None)
        .await
        .expect("api key revoked tx");
    cases.push((
        "api_key_revoked_tx",
        id,
        (ENTITY_MANAGEMENT, 4, INFORMATIONAL),
    ));

    let id = emit_agent_registered_tx(&mut tx, tenant_uuid, &identity, agent_id)
        .await
        .expect("agent registered tx");
    cases.push((
        "agent_registered_tx",
        id,
        (ACCOUNT_CHANGE, 1, INFORMATIONAL),
    ));

    let id = emit_agent_deactivated_tx(&mut tx, tenant_uuid, &identity, agent_id)
        .await
        .expect("agent deactivated tx");
    cases.push(("agent_deactivated_tx", id, (ACCOUNT_CHANGE, 3, MEDIUM)));

    let id = emit_delegation_granted_tx(&mut tx, tenant_uuid, &identity, agent_id, user_id)
        .await
        .expect("delegation granted");
    cases.push(("delegation_granted", id, (AUTHORIZATION, 1, INFORMATIONAL)));

    let id = emit_delegation_revoked_tx(&mut tx, tenant_uuid, &identity, agent_id, user_id)
        .await
        .expect("delegation revoked");
    cases.push(("delegation_revoked", id, (AUTHORIZATION, 2, INFORMATIONAL)));

    let id = emit_scim_user_created_tx(&mut tx, tenant_uuid, actor.clone(), user_id)
        .await
        .expect("scim user created");
    cases.push(("scim_user_created", id, (ACCOUNT_CHANGE, 1, INFORMATIONAL)));

    let id = emit_scim_user_updated_tx(&mut tx, tenant_uuid, actor.clone(), user_id)
        .await
        .expect("scim user updated");
    cases.push(("scim_user_updated", id, (ACCOUNT_CHANGE, 99, INFORMATIONAL)));

    let id = emit_user_deactivated_tx(&mut tx, tenant_uuid, actor.clone(), user_id)
        .await
        .expect("user deactivated");
    cases.push(("user_deactivated", id, (ACCOUNT_CHANGE, 3, MEDIUM)));

    let id = emit_user_deactivated_tx(&mut tx, tenant_uuid, actor.clone(), user_id)
        .await
        .expect("scim user deactivated");
    cases.push(("scim_user_deactivated", id, (ACCOUNT_CHANGE, 3, MEDIUM)));

    let id = emit_scim_user_deleted_tx(&mut tx, tenant_uuid, actor.clone(), user_id)
        .await
        .expect("scim user deleted");
    cases.push(("scim_user_deleted", id, (ACCOUNT_CHANGE, 4, INFORMATIONAL)));

    let group_changes = vec![
        ScimGroupAuditChange::Created { group_id },
        ScimGroupAuditChange::Updated { group_id },
        ScimGroupAuditChange::Deleted { group_id },
        ScimGroupAuditChange::MembershipAdded { group_id, user_id },
        ScimGroupAuditChange::MembershipRemoved { group_id, user_id },
        ScimGroupAuditChange::PrivilegeGranted {
            group_id,
            user_id,
            relation: "admin".to_string(),
            object: format!("tenant:{tenant_uuid}"),
        },
        ScimGroupAuditChange::PrivilegeRevoked {
            group_id,
            user_id,
            relation: "operator".to_string(),
            object: format!("tenant:{tenant_uuid}"),
        },
    ];
    let group_ids = emit_scim_group_changes_tx(&mut tx, tenant_uuid, actor.clone(), &group_changes)
        .await
        .expect("SCIM group audit batch");
    assert_eq!(group_ids.len(), 7, "one row per actual SCIM group change");
    let group_cases = [
        ("scim_group_created", (ENTITY_MANAGEMENT, 1, INFORMATIONAL)),
        ("scim_group_updated", (ENTITY_MANAGEMENT, 3, INFORMATIONAL)),
        ("scim_group_deleted", (ENTITY_MANAGEMENT, 4, INFORMATIONAL)),
        ("group_membership_added", (AUTHORIZATION, 1, INFORMATIONAL)),
        (
            "group_membership_removed",
            (AUTHORIZATION, 2, INFORMATIONAL),
        ),
        ("group_privilege_granted", (AUTHORIZATION, 1, INFORMATIONAL)),
        ("group_privilege_revoked", (AUTHORIZATION, 2, INFORMATIONAL)),
    ];
    cases.extend(
        group_ids
            .into_iter()
            .zip(group_cases)
            .map(|(id, (label, expected))| (label, id, expected)),
    );

    let id = emit_approval_decided_tx(&mut tx, tenant_uuid, actor.clone(), approval_id, true)
        .await
        .expect("approval approved");
    cases.push(("approval_approved", id, (AUTHORIZATION, 1, INFORMATIONAL)));

    let id = emit_approval_decided_tx(&mut tx, tenant_uuid, actor.clone(), approval_id, false)
        .await
        .expect("approval denied");
    cases.push(("approval_denied", id, (AUTHORIZATION, 2, LOW)));

    tx.commit().await.expect("commit emit matrix tx");

    for (label, id, expected) in cases {
        let actual = class_activity_severity(&pool, id).await;
        assert_eq!(
            actual, expected,
            "emitter `{label}` drifted from its OCSF (class_uid, activity_id, severity_id)"
        );
    }
}

#[tokio::test]
async fn scim_group_batch_uses_one_key_and_preserves_exact_change_semantics_db() {
    // Pins: a real SCIM group change set is signed with one tenant key and
    // persisted at exact lifecycle, membership, and mapped-privilege cardinality.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::new_v4();
    let actor_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let tenant_object = format!("tenant:{tenant_id}");
    let changes = vec![
        ScimGroupAuditChange::Created { group_id },
        ScimGroupAuditChange::Updated { group_id },
        ScimGroupAuditChange::Deleted { group_id },
        ScimGroupAuditChange::MembershipAdded { group_id, user_id },
        ScimGroupAuditChange::MembershipRemoved { group_id, user_id },
        ScimGroupAuditChange::PrivilegeGranted {
            group_id,
            user_id,
            relation: "admin".to_string(),
            object: tenant_object.clone(),
        },
        ScimGroupAuditChange::PrivilegeRevoked {
            group_id,
            user_id,
            relation: "operator".to_string(),
            object: tenant_object.clone(),
        },
    ];

    let mut tx = pool.begin().await.expect("begin SCIM group batch");
    let ids = emit_scim_group_changes_tx(&mut tx, tenant_id, ActorInput::user(actor_id), &changes)
        .await
        .expect("emit SCIM group batch");
    tx.commit().await.expect("commit SCIM group batch");

    assert_eq!(ids.len(), 7, "every supplied change emits exactly one row");
    let rows: Vec<ScimAuditRow> = sqlx::query_as(
        r#"
        SELECT id, class_uid, activity_id, signing_key_id, event_jcs,
               signature_hex, target_resource_uid
        FROM security_events
        WHERE id = ANY($1::uuid[])
        ORDER BY array_position($1::uuid[], id)
        "#,
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .expect("load exact SCIM group audit rows");
    assert_eq!(
        rows.len(),
        7,
        "the batch insert must not drop or duplicate rows"
    );
    let expected_class_activity = [
        (ENTITY_MANAGEMENT, 1),
        (ENTITY_MANAGEMENT, 3),
        (ENTITY_MANAGEMENT, 4),
        (AUTHORIZATION, 1),
        (AUTHORIZATION, 2),
        (AUTHORIZATION, 1),
        (AUTHORIZATION, 2),
    ];
    assert_eq!(
        rows.iter()
            .map(|row| (row.class_uid, row.activity_id))
            .collect::<Vec<_>>(),
        expected_class_activity,
        "lifecycle, membership, and privilege changes retain distinct OCSF activities"
    );
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        ids,
        "array insertion preserves the supplied change order"
    );
    let signing_key_id = rows[0].signing_key_id;
    assert!(
        rows.iter().all(|row| row.signing_key_id == signing_key_id),
        "one batch must use one signing key id"
    );
    let key_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await
            .expect("count first tenant key");
    assert_eq!(
        key_count, 1,
        "the first batch creates exactly one tenant key"
    );

    let group_target = format!("scim_group:{group_id}");
    assert!(
        rows[..5]
            .iter()
            .all(|row| row.target_resource_uid.as_deref() == Some(group_target.as_str())),
        "group and membership rows target the changed SCIM group"
    );
    assert!(
        rows[5..]
            .iter()
            .all(|row| row.target_resource_uid.as_deref() == Some(tenant_object.as_str())),
        "mapped privilege rows target the exact OpenFGA object"
    );

    let payloads: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| serde_json::from_slice(&row.event_jcs).expect("event_jcs is valid JSON"))
        .collect();
    assert_eq!(
        payloads[3]["privileges"],
        serde_json::json!([format!("member user:{user_id}")]),
        "membership grant names the exact member"
    );
    assert_eq!(
        payloads[4]["privileges"],
        serde_json::json!([format!("member user:{user_id}")]),
        "membership revoke names the exact member"
    );
    assert_eq!(
        payloads[5]["privileges"],
        serde_json::json!([
            "admin",
            format!("subject:operator:{user_id}"),
            format!("source:scim_group:{group_id}")
        ]),
        "privilege grant retains exact relation, subject, and source group"
    );
    assert_eq!(
        payloads[6]["privileges"],
        serde_json::json!([
            "operator",
            format!("subject:operator:{user_id}"),
            format!("source:scim_group:{group_id}")
        ]),
        "privilege revoke retains exact relation, subject, and source group"
    );
    assert!(
        payloads.iter().all(|payload| {
            payload
                .pointer("/actor/user/uid")
                .and_then(|value| value.as_str())
                == Some(format!("user:{actor_id}").as_str())
        }),
        "all events retain the authenticated SCIM actor"
    );
    for row in &rows {
        assert!(
            signing::verify(
                &pool,
                row.signing_key_id,
                &row.event_jcs,
                &row.signature_hex,
            )
            .await
            .expect("verify SCIM group signature"),
            "every persisted batch row must have a valid signature"
        );
    }
}

#[tokio::test]
async fn empty_scim_group_batch_creates_neither_key_nor_event_db() {
    // Pins: an unchanged PUT/PATCH can call the batch API without accidentally
    // resolving a key or leaving an audit artifact.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin empty audit batch");
    let ids = emit_scim_group_changes_tx(&mut tx, tenant_id, ActorInput::user(Uuid::new_v4()), &[])
        .await
        .expect("empty audit batch");
    tx.commit().await.expect("commit empty audit batch");
    assert!(ids.is_empty(), "empty input returns no event ids");

    let event_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM security_events WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await
            .expect("count empty batch events");
    let key_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await
            .expect("count empty batch keys");
    assert_eq!((event_count, key_count), (0, 0));
}

#[tokio::test]
async fn scim_group_batch_key_and_events_roll_back_with_caller_transaction_db() {
    // Pins: first-key creation and every signed row are in the same transaction
    // as the SCIM mutation, so a caller rollback cannot leave audit-only state.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_id = Uuid::new_v4();
    let group_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin rollback audit batch");
    let ids = emit_scim_group_changes_tx(
        &mut tx,
        tenant_id,
        ActorInput::user(Uuid::new_v4()),
        &[
            ScimGroupAuditChange::Created { group_id },
            ScimGroupAuditChange::MembershipAdded {
                group_id,
                user_id: Uuid::new_v4(),
            },
        ],
    )
    .await
    .expect("emit rollback audit batch");
    assert_eq!(ids.len(), 2);
    let in_tx: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM security_events WHERE tenant_id = $1), \
                (SELECT count(*) FROM tenant_signing_keys WHERE tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await
    .expect("read uncommitted key and events");
    assert_eq!(in_tx, (2, 1));
    tx.rollback().await.expect("roll back audit batch");

    let after: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM security_events WHERE tenant_id = $1), \
                (SELECT count(*) FROM tenant_signing_keys WHERE tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("read state after rollback");
    assert_eq!(after, (0, 0), "rollback removes both key and event rows");
}

#[tokio::test]
async fn scim_group_batch_insert_failure_rolls_back_first_key_db() {
    // Pins: an audit insertion error propagates fail-closed and the first key
    // minted earlier in the same transaction does not leak through the failure.
    let pool = support::migrated_ocsf_pool().await;
    sqlx::query(
        r#"
        CREATE FUNCTION reject_scim_group_audit_insert() RETURNS trigger AS $$
        BEGIN
            RAISE EXCEPTION 'injected SCIM audit insert failure';
        END
        $$ LANGUAGE plpgsql
        "#,
    )
    .execute(&pool)
    .await
    .expect("install isolated failure function");
    sqlx::query(
        r#"
        CREATE TRIGGER reject_scim_group_audit_insert
        BEFORE INSERT ON security_events
        FOR EACH STATEMENT EXECUTE FUNCTION reject_scim_group_audit_insert()
        "#,
    )
    .execute(&pool)
    .await
    .expect("install isolated failure trigger");

    let tenant_id = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin failing audit batch");
    let error = emit_scim_group_changes_tx(
        &mut tx,
        tenant_id,
        ActorInput::user(Uuid::new_v4()),
        &[ScimGroupAuditChange::Created {
            group_id: Uuid::new_v4(),
        }],
    )
    .await
    .expect_err("injected audit insert must fail closed");
    assert!(matches!(error, EmitError::Database(_)));
    tx.rollback().await.expect("roll back failed audit batch");

    let after: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM security_events WHERE tenant_id = $1), \
                (SELECT count(*) FROM tenant_signing_keys WHERE tenant_id = $1)",
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("read state after failed insert");
    assert_eq!(after, (0, 0), "failed insert cannot commit its first key");
}
