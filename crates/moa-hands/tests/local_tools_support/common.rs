// Shared local-tools test session fixtures.

use moa_core::{
    traits::{Identity, IdentityType},
    types::{
        contact::SessionActorRef,
        identifiers::{ModelId, TenantId, ToolCallId},
        sandbox_workspace::SandboxWorkspaceScope,
        session::SessionMeta,
    },
};

fn identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: uuid::Uuid::from_u128(0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c111),
        tenant_id: TenantId::from(uuid::Uuid::from_u128(
            0x018f_8f1f_36a6_7c90_a7f8_2f2f_57f5_c222,
        )),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn session() -> SessionMeta {
    let identity = identity();
    SessionMeta {
        tenant_id: identity.tenant_id,
        model: ModelId::new("claude-sonnet-4-6"),
        created_by: Some(SessionActorRef::Identity { id: identity.id }),
        ..SessionMeta::default()
    }
}

#[allow(dead_code)]
fn workspace_scope(session: &SessionMeta) -> SandboxWorkspaceScope {
    SandboxWorkspaceScope::Worker {
        session_id: session.id,
        worker_id: "local-tools-offline-worker".to_string(),
    }
}
