//! Session storage-partition fixture helpers.

use moa_core::{SessionMeta, StoragePartitionId};

/// Returns the storage partition id for a tenant-owned session fixture.
pub fn storage_partition_id_from_meta(meta: &SessionMeta) -> StoragePartitionId {
    StoragePartitionId::for_tenant(meta.tenant_id)
}
