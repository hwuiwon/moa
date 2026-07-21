//! Memory service authorization-scope helper coverage.

use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::memory::MemoryIngestDocument;
use moa_core::{
    types::contact::ContactId, types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_orchestrator::services::memory::{
    UserScopeError, checked_ingest_contact_id, document_ingest_session_id, effective_user_id,
};
use serde_json::json;
use uuid::Uuid;
use uuid::Variant;

fn user_identity(user_id: Uuid) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
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
fn checked_ingest_contact_id_does_not_synthesize_missing_contact() {
    // Pins: document ingestion with no contact_id remains tenant-owned instead of borrowing the caller id.
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
        checked_ingest_contact_id(None, &identity).expect("missing contact is tenant-owned"),
        None
    );
}

#[test]
fn checked_ingest_contact_id_accepts_explicit_contact_owner() {
    // Pins: contact-owned document ingestion must name the contact owner explicitly.
    let user_id =
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").expect("fixture user id parses");
    let identity = user_identity(user_id);
    let contact_id = ContactId(
        Uuid::parse_str("22222222-2222-2222-2222-222222222222").expect("fixture contact id parses"),
    );

    assert_eq!(
        checked_ingest_contact_id(Some(contact_id), &identity)
            .expect("explicit contact owner should be accepted"),
        Some(contact_id)
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

#[test]
fn document_ingest_session_id_is_stable_for_client_retries() {
    // Pins: retried document-ingest requests address the same ingestion object.
    let tenant_id =
        TenantId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("tenant id"));
    let contact_id =
        ContactId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("contact id"));
    let document = MemoryIngestDocument {
        source_name: "runbook.md".to_string(),
        content: "rotate API keys quarterly".to_string(),
        source_uri: Some("s3://docs/runbook.md".to_string()),
        metadata: json!({"ignored_for_identity": true}),
    };

    let first = document_ingest_session_id(tenant_id, Some(contact_id), 0, &document);
    let retry = document_ingest_session_id(tenant_id, Some(contact_id), 0, &document);

    assert_eq!(first, retry);
    assert_eq!(first.0.get_variant(), Variant::RFC4122);
    assert_eq!(first.0.get_version_num(), 8);
}

#[test]
fn document_ingest_session_id_separates_source_content_and_index() {
    // Pins: distinct documents in one ingest request do not share an ingestion object key.
    let tenant_id =
        TenantId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("tenant id"));
    let contact_id =
        ContactId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("contact id"));
    let document = MemoryIngestDocument {
        source_name: "runbook.md".to_string(),
        content: "rotate API keys quarterly".to_string(),
        source_uri: Some("s3://docs/runbook.md".to_string()),
        metadata: json!({}),
    };
    let changed_content = MemoryIngestDocument {
        content: "rotate API keys monthly".to_string(),
        ..document.clone()
    };
    let changed_source = MemoryIngestDocument {
        source_uri: Some("s3://docs/other.md".to_string()),
        ..document.clone()
    };

    let baseline = document_ingest_session_id(tenant_id, Some(contact_id), 0, &document);

    assert_ne!(
        baseline,
        document_ingest_session_id(tenant_id, Some(contact_id), 0, &changed_content)
    );
    assert_ne!(
        baseline,
        document_ingest_session_id(tenant_id, Some(contact_id), 0, &changed_source)
    );
    assert_ne!(
        baseline,
        document_ingest_session_id(tenant_id, Some(contact_id), 1, &document)
    );
}

#[test]
fn document_ingest_session_id_separates_tenant_and_contact_owners() {
    // Pins: tenant-owned and contact-owned document ingestion never address the same VO.
    let tenant_id =
        TenantId(Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").expect("tenant id"));
    let contact_id =
        ContactId(Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").expect("contact id"));
    let document = MemoryIngestDocument {
        source_name: "runbook.md".to_string(),
        content: "rotate API keys quarterly".to_string(),
        source_uri: Some("s3://docs/runbook.md".to_string()),
        metadata: json!({}),
    };

    assert_ne!(
        document_ingest_session_id(tenant_id, None, 0, &document),
        document_ingest_session_id(tenant_id, Some(contact_id), 0, &document)
    );
}
