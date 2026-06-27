// Base brain-turn integration-test support shared by offline and DB lanes.

use moa_core::{
    ContactId, ContactRef, ContactVerificationState, StoragePartitionId, TenantId, TokenUsage,
};
use uuid::Uuid;

fn token_usage(input_tokens: usize, output_tokens: usize) -> TokenUsage {
    TokenUsage {
        input_tokens_uncached: input_tokens,
        input_tokens_cache_write: 0,
        input_tokens_cache_read: 0,
        output_tokens,
    }
}

fn test_tenant_id() -> TenantId {
    tenant_id_from_storage_partition_id(&StoragePartitionId::new("workspace"))
}

fn test_contact_id() -> ContactId {
    contact_id_from_label("user")
}

fn test_contact_ref() -> ContactRef {
    contact_ref(test_tenant_id(), test_contact_id())
}

fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    Uuid::parse_str(storage_partition_id.as_str())
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id.as_str())))
}

fn contact_id_from_label(label: &str) -> ContactId {
    Uuid::parse_str(label)
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(label)))
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let hash = blake3::hash(label.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state: ContactVerificationState::Verified,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}
