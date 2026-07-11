// Shared local-tools test session fixtures.

use moa_core::{types::identifiers::ModelId, types::contact::SessionActorRef, types::session::SessionMeta, types::identifiers::TenantId};

fn session() -> SessionMeta {
    SessionMeta {
        tenant_id: TenantId::new(),
        model: ModelId::new("claude-sonnet-4-6"),
        created_by: Some(SessionActorRef::Identity {
            id: uuid::Uuid::now_v7(),
        }),
        ..SessionMeta::default()
    }
}
