//! Session metadata fixture for Restate service tests.

use moa_core::{
    AgentContext, AgentKnowledgePolicy, AgentKnowledgeScopeMode, AgentPolicySnapshot, ContactId,
    ContactRef, ContactVerificationState, ModelId, SessionMeta, TenantId,
};

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
    ContactRef {
        contact_id: ContactId::new(),
        tenant_id,
        state: ContactVerificationState::Unverified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::json!({}),
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}
