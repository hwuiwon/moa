//! Offline validation coverage for typed authz administration requests.

use moa_orchestrator::services::authz_admin::WriteTupleRequest;
use serde_json::json;

#[test]
fn raw_tuple_body_is_rejected_offline() {
    // Pins: arbitrary OpenFGA tuple strings are not accepted by the Authz service payload.
    let body = json!({
        "user": "api_key:11111111-1111-1111-1111-111111111111",
        "relation": "admin",
        "object": "tenant:22222222-2222-2222-2222-222222222222",
    });

    let error = serde_json::from_value::<WriteTupleRequest>(body)
        .expect_err("raw tuple bodies must not deserialize");

    assert!(
        error.to_string().contains("missing field `operation`"),
        "raw tuple body should fail on the missing typed operation, got {error}"
    );
}

#[test]
fn raw_session_participant_tuple_body_is_rejected_offline() {
    // Pins: public tuple administration cannot grant session participation through raw tuple strings.
    let body = json!({
        "user": "operator:11111111-1111-1111-1111-111111111111",
        "relation": "participant",
        "object": "session:22222222-2222-2222-2222-222222222222",
        "tenant_id": "33333333-3333-3333-3333-333333333333",
    });

    let error = serde_json::from_value::<WriteTupleRequest>(body)
        .expect_err("raw session participant tuple bodies must not deserialize");

    assert!(
        error.to_string().contains("missing field `operation`"),
        "raw session participant tuple body should fail before any tuple write, got {error}"
    );
}

#[test]
fn stale_user_subject_is_rejected_offline() {
    // Pins: stale `user:<id>` subject tuples cannot be accepted as public authz writes.
    let body = json!({
        "operation": "grant_api_key_tenant_role",
        "user": "user:11111111-1111-1111-1111-111111111111",
        "relation": "admin",
        "object": "tenant:22222222-2222-2222-2222-222222222222",
    });

    let error = serde_json::from_value::<WriteTupleRequest>(body)
        .expect_err("stale user tuple bodies must not deserialize");

    assert!(
        error.to_string().contains("unknown field `user`"),
        "stale user tuple should fail on the subject field, got {error}"
    );
}

#[test]
fn unsupported_relation_is_rejected_offline() {
    // Pins: public authz writes allow only API-key admin/operator tenant roles.
    let body = json!({
        "operation": "grant_api_key_tenant_role",
        "api_key_id": "11111111-1111-1111-1111-111111111111",
        "tenant_id": "22222222-2222-2222-2222-222222222222",
        "relation": "participant",
    });

    let error = serde_json::from_value::<WriteTupleRequest>(body)
        .expect_err("unsupported relations must not deserialize");

    assert!(
        error.to_string().contains("unknown variant `participant`"),
        "unsupported relation should fail on the relation allowlist, got {error}"
    );
}

#[test]
fn unsupported_operation_is_rejected_offline() {
    // Pins: session/agent tuple operations cannot be represented in the public request enum.
    let body = json!({
        "operation": "grant_session_participant",
        "api_key_id": "11111111-1111-1111-1111-111111111111",
        "tenant_id": "22222222-2222-2222-2222-222222222222",
        "relation": "admin",
    });

    let error = serde_json::from_value::<WriteTupleRequest>(body)
        .expect_err("unsupported operations must not deserialize");

    assert!(
        error
            .to_string()
            .contains("unknown variant `grant_session_participant`"),
        "unsupported tuple operation should fail on the operation tag, got {error}"
    );
}
