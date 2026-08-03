//! Unit tests for Session handler routing and helper invariants.
use moa_core::{
    types::channel::Channel, types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::identifiers::ModelId,
    types::identifiers::SessionId, types::identifiers::TenantId,
    types::security::SecurityCircuitOwner, types::session::SessionMeta,
};
use restate_sdk::prelude::TerminalError;

use super::progress::active_turn_progress_or_none;
use super::{
    SessionVoState, WorkerInputTarget, activate_coordinator_security_owner,
    admitted_contact_for_turn, pending_message_queue_is_full, worker_provide_input_request,
};

#[test]
fn coordinator_turn_admission_installs_the_security_owner() {
    // Pins: the owner fence exists before any classified tool output or
    // delayed action-review assessment can reach the Session VO.
    let mut state = SessionVoState::default();

    activate_coordinator_security_owner(&mut state, "turn-7", 7);

    assert_eq!(
        state.security_circuit.owner,
        Some(SecurityCircuitOwner::Coordinator {
            turn_id: "turn-7".to_string(),
            generation: 7,
        })
    );
}

#[test]
fn pending_message_queue_rejects_exactly_at_the_configured_bound() {
    // Pins: active sessions accept only the declared number of queued
    // messages; the next message is rejected instead of growing state.
    assert!(!pending_message_queue_is_full(7, 8));
    assert!(pending_message_queue_is_full(8, 8));
    assert!(pending_message_queue_is_full(9, 8));
}

#[test]
fn session_worker_reply_payload_carries_exact_parent_session_and_string() {
    // Pins: Session plain-reply routing sends the exact owning Session scope, the full
    // owner fence of the advertised target, and keeps the canonical Value::String
    // payload expected by Worker replay hashing.
    let parent_session = SessionId::new();
    let target = WorkerInputTarget {
        turn_id: "worker-turn-9".to_string(),
        generation: 5,
        input_request_id: "request-9".to_string(),
    };
    let request = worker_provide_input_request(parent_session, target.clone(), "the exact answer");

    assert_eq!(request.parent_session, parent_session);
    assert_eq!(request.target, target);
    assert_eq!(
        request.input,
        serde_json::Value::String("the exact answer".to_string())
    );
}

#[test]
fn session_progress_active_turn_failure_returns_none() {
    // Pins: Session/progress still returns snapshot and durable history when the active turn workflow is unavailable.
    let progress = active_turn_progress_or_none(
        "turn-1",
        Err(TerminalError::new("turn progress unavailable")),
    );

    assert_eq!(progress, None);
}

#[test]
fn admitted_contact_for_turn_rejects_per_message_contact_override() {
    // Pins: contact context for turns comes from persisted SessionMeta, not caller payloads.
    let tenant_id = TenantId::new();
    let session_contact = contact(ContactId::new(), tenant_id);
    let requested_contact = contact(ContactId::new(), tenant_id);
    let meta = session_meta(session_contact.clone());

    let error = admitted_contact_for_turn(Some(requested_contact), &meta)
        .expect_err("mismatched contact override should fail");

    assert!(
        format!("{error:?}").contains("turn contact override is not allowed"),
        "unexpected error: {error:?}"
    );
    assert_eq!(
        admitted_contact_for_turn(Some(session_contact.clone()), &meta)
            .expect("matching snapshot should be admitted"),
        Some(session_contact)
    );
    assert_eq!(
        admitted_contact_for_turn(None, &meta).expect("missing contact should use session"),
        meta.contact
    );
}

fn session_meta(contact: ContactRef) -> SessionMeta {
    SessionMeta {
        tenant_id: contact.tenant_id,
        channel: Channel::Chat,
        model: ModelId::new("mock"),
        contact: Some(contact),
        ..SessionMeta::default()
    }
}

fn contact(contact_id: ContactId, tenant_id: TenantId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Unverified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}
