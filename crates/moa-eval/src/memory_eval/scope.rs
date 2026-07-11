//! Eval-only storage-partition scope conversion helpers.

use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::TenantId};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Maps a storage partition identifier to the tenant ID used by memory eval fixtures.
#[must_use]
pub fn tenant_id_from_storage_partition_id(storage_partition_id: &StoragePartitionId) -> TenantId {
    tenant_id_from_storage_partition(storage_partition_id.as_str())
}

/// Maps a storage partition label to the tenant ID used by memory eval fixtures.
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
    use super::{tenant_id_from_storage_partition, tenant_id_from_storage_partition_id};
    use moa_core::{types::identifiers::StoragePartitionId, types::identifiers::TenantId};
    use uuid::Uuid;

    #[test]
    fn uuid_storage_partition_round_trips_to_tenant_id() {
        // Pins: UUID storage partitions are treated as the canonical tenant UUID in eval scope.
        let tenant_uuid = Uuid::parse_str("018f8f1f-36a6-7c90-a7f8-2f2f57f5c222")
            .expect("fixture tenant UUID should parse");
        let tenant_id = TenantId::from(tenant_uuid);
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);

        assert_eq!(
            tenant_id_from_storage_partition_id(&storage_partition_id),
            tenant_id
        );
    }

    #[test]
    fn label_storage_partition_maps_to_stable_tenant_id() {
        // Pins: non-UUID eval storage labels map to deterministic tenant IDs.
        let tenant_id = tenant_id_from_storage_partition("tenant-payments");
        let expected = TenantId::from(
            Uuid::parse_str("6bccab23-77b2-8396-94ed-1bb14eb4ee59")
                .expect("fixture deterministic tenant UUID should parse"),
        );

        assert_eq!(tenant_id, expected);
    }
}
