//! Memory service authorization-scope helper coverage.

use moa_core::traits::{Identity, IdentityType};
use moa_core::{ContactId, MemoryScope, TenantId, UserId};
use moa_orchestrator::services::memory::{
    UserScopeError, checked_ingest_contact_id, checked_memory_scope, effective_user_id,
};
use uuid::Uuid;

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: user_id,
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn agent_identity(agent_id: Uuid, acting_on_behalf_of: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Agent,
        id: agent_id,
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: Some(acting_on_behalf_of),
    }
}

#[test]
fn contact_memory_scope_uses_requested_contact_inside_tenant() {
    // Pins: caller-supplied contact_id builds a contact-local memory scope inside the tenant.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = user_identity(user_id);
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();

    let scope = checked_memory_scope(tenant_id, Some(contact_id), &identity)
        .expect("admin contact scope should be accepted");

    assert_eq!(
        scope,
        MemoryScope::Contact {
            tenant_id,
            contact_id,
        }
    );
}

#[test]
fn checked_memory_scope_rejects_mismatched_contact_identity() {
    // Pins: contact callers cannot request another contact's memory scope.
    let contact_uuid =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let other_id = ContactId(
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("fixture contact id parses"),
    );
    let identity = Identity {
        identity_type: IdentityType::Contact,
        id: contact_uuid,
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };

    let error = checked_memory_scope(identity.tenant_id, Some(other_id), &identity)
        .expect_err("mismatched contact scope should be rejected");

    assert_eq!(
        error,
        UserScopeError::Mismatch {
            requested: UserId::new(other_id.to_string()),
            effective: contact_uuid.to_string(),
        }
    );
}

#[test]
fn checked_ingest_contact_id_uses_contact_identity() {
    // Pins: document ingestion attribution comes from the trusted contact identity when absent.
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
        checked_ingest_contact_id(None, &identity).expect("identity id should be used"),
        ContactId(agent_id)
    );
}

#[test]
fn checked_ingest_contact_id_rejects_payload_contact_mismatch() {
    // Pins: document ingestion cannot attribute a contact caller to a different contact id.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let requested = ContactId(
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("fixture contact id parses"),
    );
    let identity = Identity {
        identity_type: IdentityType::Contact,
        id: user_id,
        tenant_id: TenantId::new(),
        api_key_id: None,
        acting_on_behalf_of: None,
    };

    let error = checked_ingest_contact_id(Some(requested), &identity)
        .expect_err("mismatched ingestion contact should be rejected");

    assert_eq!(
        error,
        UserScopeError::Mismatch {
            requested: UserId::new(requested.to_string()),
            effective: user_id.to_string(),
        }
    );
}
