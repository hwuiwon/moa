//! Shared test fixtures and small deterministic helpers.
//!
//! These helpers centralize logic that was previously copy-pasted across many
//! crate test suites: SQL identifier quoting, deterministic tenant/UUID
//! derivation from fixture labels, and minimal `ContactRef`/`SessionMeta`
//! builders. Keeping a single copy here keeps behavior identical across lanes.

use moa_core::{
    types::contact::ContactId, types::contact::ContactRef,
    types::contact::ContactVerificationState, types::contact::SessionActorRef,
    types::identifiers::ModelId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::session::SessionMeta,
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
///
/// This is THE workspace-wide stable-UUID-from-label helper for tests and eval
/// tooling: given the same label it always returns the same UUID, and every
/// caller that needs a deterministic UUID from a fixture label must route
/// through this function so derived tenant/contact IDs stay identical across
/// crates and lanes. Do not fork this logic (previous copies used a different
/// hash and produced divergent UUIDs); import it here instead.
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

#[cfg(test)]
mod tests {
    use super::{stable_uuid_from_label, tenant_id_from_storage_partition};
    use moa_core::types::identifiers::{StoragePartitionId, TenantId};
    use uuid::Uuid;

    #[test]
    fn uuid_storage_partition_round_trips_to_tenant_id() {
        // Pins: a UUID storage partition is treated as the canonical tenant UUID.
        let tenant_uuid = Uuid::parse_str("018f8f1f-36a6-7c90-a7f8-2f2f57f5c222")
            .expect("fixture tenant UUID should parse");
        let tenant_id = TenantId::from(tenant_uuid);
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);

        assert_eq!(
            super::tenant_id_from_storage_partition_id(&storage_partition_id),
            tenant_id
        );
    }

    #[test]
    fn label_storage_partition_maps_to_stable_tenant_id() {
        // Pins: non-UUID storage labels derive a stable tenant ID from the label hash.
        let tenant_id = tenant_id_from_storage_partition("tenant-payments");
        let expected = TenantId::from(
            Uuid::parse_str("6bccab23-77b2-8396-94ed-1bb14eb4ee59")
                .expect("fixture deterministic tenant UUID should parse"),
        );

        assert_eq!(tenant_id, expected);
    }

    #[test]
    fn stable_uuid_is_deterministic_for_label() {
        // Pins: the same label always derives the same UUID.
        assert_eq!(
            stable_uuid_from_label("tenant-payments"),
            stable_uuid_from_label("tenant-payments")
        );
    }
}
