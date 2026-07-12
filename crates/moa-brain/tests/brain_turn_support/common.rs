// Base brain-turn integration-test support shared by offline and DB lanes.

use moa_core::{
    types::contact::ContactId, types::contact::ContactRef, types::contact::ContactVerificationState, types::identifiers::StoragePartitionId, types::identifiers::TenantId, types::completion::TokenUsage,
};
use moa_test_support::fixtures::{contact_ref_fixture, stable_uuid_from_label};
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

fn contact_ref(tenant_id: TenantId, contact_id: ContactId) -> ContactRef {
    contact_ref_fixture(contact_id, tenant_id, ContactVerificationState::Verified)
}
