//! SessionStore request and metadata fixtures for Restate service tests.

use moa_core::{
    events::Event, types::agent::AgentContext, types::agent::AgentKnowledgePolicy,
    types::agent::AgentKnowledgeScopeMode, types::agent::AgentPolicySnapshot,
    types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::events_stream::EventRange,
    types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::session::SessionMeta,
};
use moa_test_support::fixtures::contact_ref_fixture;
use moa_wire::session_store::{AppendEventRequest, GetEventsRequest, InitSessionVoRequest};
use moa_wire::turn::StartTurnRequest;

/// Returns a request payload for `append_event`.
pub fn append_event_request(session_id: SessionId, event: Event) -> AppendEventRequest {
    AppendEventRequest {
        session_id,
        event,
        dedupe_key: None,
    }
}

/// Returns a request payload for `get_events`.
pub fn get_events_request(session_id: SessionId, range: EventRange) -> GetEventsRequest {
    GetEventsRequest { session_id, range }
}

/// Returns a request payload for `init_session_vo`.
pub fn init_session_vo_request(session_id: SessionId, meta: SessionMeta) -> InitSessionVoRequest {
    InitSessionVoRequest { session_id, meta }
}

/// Returns a `Session/start_turn` payload carrying a fresh caller retry identity.
///
/// Every call mints a new client message id, which is what a real client does for a new
/// message. A fixture that reused one id across messages would be answered by the
/// admission fence with the first message's response instead of starting a turn.
pub fn start_turn_request(text: impl Into<String>) -> StartTurnRequest {
    StartTurnRequest {
        client_message_id: moa_core::types::contact::ClientMessageId::new(
            uuid::Uuid::now_v7().to_string(),
        )
        .expect("a uuid is a valid client message id"),
        reply_to: None,
        stream_cursor: None,
        user_message: text.into(),
        attachments: Vec::new(),
        model: None,
        contact: None,
        max_turns: None,
        execution_template: None,
    }
}

/// Returns a user-message event suitable for append-event tests.
pub fn user_message_event(text: impl Into<String>) -> Event {
    Event::UserMessage {
        text: text.into(),
        attachments: vec![],
    }
}

/// Returns the storage partition id for a tenant-owned session fixture.
pub fn storage_partition_id_from_meta(meta: &SessionMeta) -> StoragePartitionId {
    StoragePartitionId::for_tenant(meta.tenant_id)
}

/// Returns a session metadata payload suitable for `create_session`.
pub fn test_session_meta(storage_partition_id: &str) -> SessionMeta {
    let _ = storage_partition_id;
    let tenant_id = TenantId::new();
    SessionMeta {
        tenant_id,
        contact: Some(test_contact_ref(tenant_id)),
        model: ModelId::new("test-model"),
        agent_context: Some(test_agent_context()),
        ..SessionMeta::default()
    }
}

fn test_agent_context() -> AgentContext {
    let snapshot = AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    };
    let mut context = AgentContext::system_default();
    context.policy_snapshot = serde_json::json!(snapshot);
    context
}

fn test_contact_ref(tenant_id: TenantId) -> ContactRef {
    let mut contact = contact_ref_fixture(
        ContactId::new(),
        tenant_id,
        ContactVerificationState::Unverified,
    );
    contact.permissions = serde_json::json!({});
    contact
}
