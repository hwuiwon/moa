//! Integration tests for the memory data-access transparency OCSF event.
//!
//! These drive `emit_data_access` (the durable variant) against a migrated
//! `security_events` schema and pin the persisted Datastore Activity (6005)
//! classification, the queryable actor/session/scope columns, the summary
//! access detail carried in `event_jcs`, and HMAC verifiability. A zero-record
//! retrieval must still persist an access attempt.

use moa_core::{
    traits::{Identity, IdentityType},
    types::agent::AgentContext,
    types::identifiers::{SessionId, TenantId},
    types::session::SessionMeta,
};
use moa_ocsf::{MemoryDataAccess, MemoryDataAccessDetails, emit_data_access, signing};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use super::support;

/// OCSF constants persisted by the data-access emitter.
const DATASTORE_ACTIVITY: i32 = 6005;
const APPLICATION_ACTIVITY: i32 = 6;
const READ: i32 = 1;
const INFORMATIONAL: i32 = 1;

struct PersistedEvent {
    class_uid: i32,
    activity_id: i32,
    category_uid: i32,
    severity_id: i32,
    type_uid: i64,
    actor_user_uid: Option<String>,
    actor_session_uid: Option<String>,
    target_resource_uid: Option<String>,
    event_jcs: Vec<u8>,
    signing_key_id: Uuid,
    signature_hex: String,
}

fn session_meta(tenant_id: TenantId, session_id: Uuid, agent_id: Option<Uuid>) -> SessionMeta {
    let agent_context = agent_id.map(|agent_id| AgentContext {
        agent_id: Some(agent_id),
        ..AgentContext::system_default()
    });
    SessionMeta {
        id: SessionId(session_id),
        tenant_id,
        agent_context,
        ..SessionMeta::default()
    }
}

/// Raw column tuple for a persisted `security_events` row under test.
type EventRow = (
    i32,
    i32,
    i32,
    i32,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Vec<u8>,
    Uuid,
    String,
);

async fn load_event(pool: &PgPool, id: Uuid) -> PersistedEvent {
    let row: EventRow = sqlx::query_as(
        r#"
        SELECT class_uid, activity_id, category_uid, severity_id, type_uid,
               actor_user_uid, actor_session_uid, target_resource_uid,
               event_jcs, signing_key_id, signature_hex
        FROM security_events
        WHERE id = $1
        "#,
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("data-access security event row");
    PersistedEvent {
        class_uid: row.0,
        activity_id: row.1,
        category_uid: row.2,
        severity_id: row.3,
        type_uid: row.4,
        actor_user_uid: row.5,
        actor_session_uid: row.6,
        target_resource_uid: row.7,
        event_jcs: row.8,
        signing_key_id: row.9,
        signature_hex: row.10,
    }
}

#[tokio::test]
async fn emit_data_access_persists_queryable_datastore_read_summary_db() {
    // Pins: the memory data-access emitter records a Datastore Activity (6005)
    // "Read" (1) event at Informational severity in the Application Activity (6)
    // category, exposes who/which-session/which-scope as queryable columns, carries
    // the summary count and tiers in event_jcs without any resource name, and stays
    // HMAC-verifiable.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_uuid = Uuid::from_u128(0x6001);
    let contact_id = Uuid::from_u128(0x6002);
    let session_id = Uuid::from_u128(0x6003);
    let turn_id = Uuid::from_u128(0x6004);
    let agent_id = Uuid::from_u128(0x6005);
    let api_key_id = Uuid::from_u128(0x6006);
    let delegated_principal_id = Uuid::from_u128(0x6007);
    let low_uid = Uuid::from_u128(0x10);
    let high_uid = Uuid::from_u128(0x20);
    let scope_uid = format!("memory:contact:{tenant_uuid}:{contact_id}");
    let tenant_id = TenantId::from(tenant_uuid);
    let identity = Identity {
        identity_type: IdentityType::Agent,
        id: contact_id,
        tenant_id,
        api_key_id: Some(api_key_id),
        acting_on_behalf_of: Some(delegated_principal_id),
    };
    let session = session_meta(tenant_id, session_id, Some(agent_id));

    let event_id = emit_data_access(
        &pool,
        tenant_id,
        MemoryDataAccess::from_session(
            &identity,
            &session,
            MemoryDataAccessDetails {
                retrieval_operation_id: format!("turn:{turn_id}"),
                node_uids: vec![high_uid, low_uid, high_uid],
                scope_uid: scope_uid.clone(),
                scope_tier: "contact".to_string(),
                source_tiers: vec![
                    "tenant_knowledge".to_string(),
                    "user_memory".to_string(),
                    "tenant_knowledge".to_string(),
                ],
                turn_uid: Some(format!("turn:{turn_id}")),
            },
        ),
    )
    .await
    .expect("emit data access");

    let event = load_event(&pool, event_id).await;
    assert_eq!(event.class_uid, DATASTORE_ACTIVITY);
    assert_eq!(event.activity_id, READ);
    assert_eq!(event.category_uid, APPLICATION_ACTIVITY);
    assert_eq!(event.severity_id, INFORMATIONAL);
    assert_eq!(
        event.type_uid,
        i64::from(DATASTORE_ACTIVITY * 100 + READ),
        "type_uid = class*100+activity"
    );
    assert_eq!(
        event.actor_user_uid.as_deref(),
        Some(format!("agent:{contact_id}").as_str()),
        "the accessing principal is queryable"
    );
    assert_eq!(
        event.actor_session_uid.as_deref(),
        Some(format!("session:{session_id}").as_str()),
        "the accessing session is queryable"
    );
    assert_eq!(
        event.target_resource_uid.as_deref(),
        Some(scope_uid.as_str()),
        "the accessed scope is the queryable target resource"
    );

    let payload: Value = serde_json::from_slice(&event.event_jcs).expect("event_jcs is JSON");
    assert_eq!(payload["access"]["records_returned"], Value::from(2));
    assert_eq!(
        payload["access"]["retrieval_operation_id"],
        Value::from(format!("turn:{turn_id}"))
    );
    assert_eq!(
        payload["access"]["node_uids"],
        serde_json::json!([low_uid.to_string(), high_uid.to_string()]),
        "exact node UIDs must be sorted and deduplicated"
    );
    assert_eq!(payload["access"]["scope_tier"], Value::from("contact"));
    assert_eq!(
        payload["access"]["storage_partition"],
        Value::from(tenant_uuid.to_string())
    );
    assert_eq!(
        payload["access"]["source_tiers"],
        serde_json::json!(["tenant_knowledge", "user_memory"])
    );
    assert_eq!(
        payload["access"]["turn_uid"],
        Value::from(format!("turn:{turn_id}"))
    );
    assert_eq!(
        payload["access"]["agent_uid"],
        Value::from(format!("agent:{agent_id}"))
    );
    assert_eq!(
        payload["access"]["api_key_uid"],
        Value::from(format!("api_key:{api_key_id}"))
    );
    assert_eq!(
        payload["access"]["acting_on_behalf_of_uid"],
        Value::from(format!("principal:{delegated_principal_id}"))
    );
    assert!(
        payload["resource"].get("name").is_none(),
        "the accessed resource must never carry a node name: {}",
        payload["resource"]
    );

    let verified = signing::verify(
        &pool,
        event.signing_key_id,
        &event.event_jcs,
        &event.signature_hex,
    )
    .await
    .expect("verify signature");
    assert!(verified, "data-access audit signature must verify");

    let update_error = sqlx::query("UPDATE security_events SET severity_id = 6 WHERE id = $1")
        .bind(event_id)
        .execute(&pool)
        .await
        .expect_err("signed security evidence must reject ordinary updates");
    assert_eq!(
        update_error
            .as_database_error()
            .and_then(|error| error.code().map(|code| code.into_owned()))
            .as_deref(),
        Some("55000"),
        "signed security evidence must fail with object-not-in-prerequisite-state"
    );
}

#[tokio::test]
async fn emit_data_access_records_zero_record_retrieval_attempt_db() {
    // Pins: a retrieval that returned no records still persists an access attempt
    // with records_returned = 0, so the audit trail shows the read happened.
    let pool = support::migrated_ocsf_pool().await;
    let tenant_uuid = Uuid::from_u128(0x6101);
    let scope_uid = format!("memory:tenant:{tenant_uuid}");
    let operation_id = format!("service:{}", Uuid::from_u128(0x6104));
    let tenant_id = TenantId::from(tenant_uuid);
    let identity = Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x6102),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let session = session_meta(tenant_id, Uuid::from_u128(0x6103), None);

    let event_id = emit_data_access(
        &pool,
        tenant_id,
        MemoryDataAccess::from_session(
            &identity,
            &session,
            MemoryDataAccessDetails {
                retrieval_operation_id: operation_id.clone(),
                node_uids: Vec::new(),
                scope_uid: scope_uid.clone(),
                scope_tier: "tenant".to_string(),
                source_tiers: vec!["tenant_knowledge".to_string()],
                turn_uid: None,
            },
        ),
    )
    .await
    .expect("emit zero-record data access");

    let event = load_event(&pool, event_id).await;
    assert_eq!(event.class_uid, DATASTORE_ACTIVITY);
    assert_eq!(
        event.target_resource_uid.as_deref(),
        Some(scope_uid.as_str())
    );
    let payload: Value = serde_json::from_slice(&event.event_jcs).expect("event_jcs is JSON");
    assert_eq!(
        payload["access"]["records_returned"],
        Value::from(0),
        "zero-record retrieval still records an access attempt"
    );

    let replay_id = emit_data_access(
        &pool,
        tenant_id,
        MemoryDataAccess::from_session(
            &identity,
            &session,
            MemoryDataAccessDetails {
                retrieval_operation_id: operation_id.clone(),
                node_uids: Vec::new(),
                scope_uid,
                scope_tier: "tenant".to_string(),
                source_tiers: vec!["tenant_knowledge".to_string()],
                turn_uid: None,
            },
        ),
    )
    .await
    .expect("replay zero-record data access");
    assert_eq!(replay_id, event_id, "replay must return the existing event");

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM security_events WHERE tenant_id = $1 AND retrieval_operation_id = $2",
    )
    .bind(tenant_uuid)
    .bind(operation_id)
    .fetch_one(&pool)
    .await
    .expect("count replay-idempotent event");
    assert_eq!(event_count, 1, "one logical retrieval emits one event");
}
