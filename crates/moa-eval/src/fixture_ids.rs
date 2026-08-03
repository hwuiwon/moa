//! Deterministic identifiers owned by the evaluation runtime.

use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Maps a storage partition identifier to the tenant ID used by eval fixtures.
#[must_use]
pub fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    tenant_id_from_storage_partition(storage_partition_id.as_str())
}

/// Maps a storage partition label to the tenant ID used by eval fixtures.
#[must_use]
pub fn tenant_id_from_storage_partition(storage_partition: &str) -> TenantId {
    Uuid::parse_str(storage_partition)
        .map(TenantId::from)
        .unwrap_or_else(|_| tenant_id_from_label(storage_partition))
}

/// Maps an arbitrary eval fixture label to a deterministic tenant ID.
#[must_use]
pub fn tenant_id_from_label(label: &str) -> TenantId {
    TenantId::from(stable_uuid_from_label(label))
}

/// Maps an arbitrary eval fixture label to a deterministic UUID.
#[must_use]
pub fn stable_uuid_from_label(label: &str) -> Uuid {
    let digest = Sha256::digest(label.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::{stable_uuid_from_label, tenant_id_from_storage_partition};
    use moa_core::types::identifiers::TenantId;
    use uuid::Uuid;

    #[test]
    fn tenant_payments_label_keeps_the_established_fixture_identity() {
        // Pins: moving eval fixture IDs out of moa-test-support does not alter
        // existing corpus tenant, contact, session, or fact identities.
        let expected = Uuid::parse_str("6bccab23-77b2-8396-94ed-1bb14eb4ee59")
            .expect("known fixture UUID should parse");

        assert_eq!(stable_uuid_from_label("tenant-payments"), expected);
        assert_eq!(
            tenant_id_from_storage_partition("tenant-payments"),
            TenantId::from(expected)
        );
    }
}
