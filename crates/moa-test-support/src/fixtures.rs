//! Shared test fixtures and small deterministic helpers.
//!
//! These helpers centralize logic that was previously copy-pasted across many
//! crate test suites: SQL identifier quoting, deterministic tenant/UUID
//! derivation from fixture labels, and minimal `ContactRef`/`SessionMeta`
//! builders. Keeping a single copy here keeps behavior identical across lanes.

use moa_core::{
    ContactId, ContactRef, ContactVerificationState, ModelId, SessionActorRef, SessionMeta,
    StoragePartitionId, TenantId,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Quotes a SQL identifier by wrapping it in double quotes and escaping any
/// embedded double quotes.
#[must_use]
pub fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Maps a storage partition identifier to the tenant ID used by test fixtures.
#[must_use]
pub fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    tenant_id_from_storage_partition(storage_partition_id.as_str())
}

/// Maps a storage partition label to the tenant ID used by test fixtures.
#[must_use]
pub fn tenant_id_from_storage_partition(storage_partition: &str) -> TenantId {
    Uuid::parse_str(storage_partition)
        .map(TenantId::from)
        .unwrap_or_else(|_| tenant_id_from_label(storage_partition))
}

/// Maps an arbitrary fixture label to a deterministic tenant ID.
#[must_use]
pub fn tenant_id_from_label(label: &str) -> TenantId {
    TenantId::from(stable_uuid_from_label(label))
}

/// Maps an arbitrary fixture label to a deterministic UUID.
#[must_use]
pub fn stable_uuid_from_label(label: &str) -> Uuid {
    let digest = Sha256::digest(label.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

/// Builds a minimal `ContactRef` with the supplied identity and assurance
/// state and all other fields defaulted to empty.
#[must_use]
pub fn contact_ref_fixture(
    contact_id: ContactId,
    tenant_id: TenantId,
    state: ContactVerificationState,
) -> ContactRef {
    ContactRef {
        contact_id,
        tenant_id,
        state,
        canonical_contact_id: None,
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    }
}

/// Builds a minimal valid `SessionMeta` for the supplied tenant.
///
/// Intentionally omits contact and agent context so tests can extend it.
#[must_use]
pub fn session_meta_fixture(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        tenant_id,
        created_by: Some(SessionActorRef::Identity {
            id: Uuid::from_u128(1),
        }),
        model: ModelId::new("test-model"),
        ..SessionMeta::default()
    }
}
