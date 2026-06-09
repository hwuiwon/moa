//! Memory service authorization-scope helper coverage.

use moa_core::traits::{Identity, IdentityType};
use moa_core::{MemoryScope, UserId, WorkspaceId};
use moa_orchestrator::services::memory::{
    UserScopeError, checked_ingest_user_id, checked_memory_scope, effective_user_id,
};
use uuid::Uuid;

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: user_id,
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn agent_identity(agent_id: Uuid, acting_on_behalf_of: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Agent,
        id: agent_id,
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: Some(acting_on_behalf_of),
    }
}

#[test]
fn checked_memory_scope_accepts_only_trusted_user_identity() {
    // Pins: caller-supplied user_id is only a consistency check against trusted identity headers.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = user_identity(user_id);
    let workspace_id = WorkspaceId::new("workspace-a");

    let scope = checked_memory_scope(
        workspace_id.clone(),
        Some(UserId::new(user_id.to_string())),
        &identity,
    )
    .expect("matching user scope should be accepted");

    assert_eq!(
        scope,
        MemoryScope::User {
            workspace_id,
            user_id: UserId::new(user_id.to_string())
        }
    );
}

#[test]
fn checked_memory_scope_rejects_mismatched_user_id() {
    // Pins: memory service rejects user impersonation in payloads before building a user scope.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let other_id = UserId::new("22222222-2222-2222-2222-222222222222");
    let identity = user_identity(user_id);

    let error = checked_memory_scope(
        WorkspaceId::new("workspace-a"),
        Some(other_id.clone()),
        &identity,
    )
    .expect_err("mismatched user scope should be rejected");

    assert_eq!(
        error,
        UserScopeError::Mismatch {
            requested: other_id,
            effective: user_id.to_string(),
        }
    );
}

#[test]
fn checked_ingest_user_id_uses_agent_delegation() {
    // Pins: document ingestion attribution comes from trusted agent delegation when present.
    let agent_id =
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("fixture agent id parses");
    let acting_user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = agent_identity(agent_id, acting_user_id);

    assert_eq!(
        effective_user_id(&identity),
        Some(UserId::new(acting_user_id.to_string()))
    );
    assert_eq!(
        checked_ingest_user_id(None, &identity).expect("delegated user should be used"),
        UserId::new(acting_user_id.to_string())
    );
}

#[test]
fn checked_ingest_user_id_rejects_payload_user_mismatch() {
    // Pins: document ingestion cannot attribute work to a caller-supplied different user id.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let requested = UserId::new("22222222-2222-2222-2222-222222222222");
    let identity = user_identity(user_id);

    let error = checked_ingest_user_id(Some(&requested), &identity)
        .expect_err("mismatched ingestion user should be rejected");

    assert_eq!(
        error,
        UserScopeError::Mismatch {
            requested,
            effective: user_id.to_string(),
        }
    );
}
