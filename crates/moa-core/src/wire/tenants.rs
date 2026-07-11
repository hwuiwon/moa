//! Shared wire types for tenant lifecycle operations.

use serde::{Deserialize, Serialize};

use crate::types::identifiers::TenantId;

const TENANT_PURGE_OPERATION_PREFIX: &str = "tenant-purge-";

/// Request to start the one durable purge owned by a tenant workflow key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPurgeRequest {
    /// Tenant whose product data must be purged.
    pub tenant_id: TenantId,
}

/// Request to read a tenant purge workflow's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPurgeStatusRequest {
    /// Tenant workflow key expected by the status handler.
    pub tenant_id: TenantId,
}

/// Durable states exposed for a tenant purge operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TenantPurgeStatus {
    /// The workflow is admitted but relational deletion has not committed.
    Pending,
    /// Product rows and inverse authorization tuples committed atomically.
    RelationallyCommitted,
    /// Configured ClickHouse lineage and analytics rows were purged.
    AnalyticsPurged,
    /// A non-retryable workflow invariant prevented the purge from continuing.
    FailedTerminal,
}

/// Public projection of a tenant purge workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantPurgeStatusResponse {
    /// Stable identifier derived from the tenant-keyed workflow.
    pub operation_id: String,
    /// Current durable purge state.
    pub status: TenantPurgeStatus,
}

/// Returns the stable public operation identifier for a tenant purge workflow.
#[must_use]
pub fn tenant_purge_operation_id(tenant_id: TenantId) -> String {
    format!("{TENANT_PURGE_OPERATION_PREFIX}{tenant_id}")
}

/// Extracts the tenant workflow key from a stable purge operation identifier.
#[must_use]
pub fn tenant_id_from_purge_operation_id(operation_id: &str) -> Option<TenantId> {
    operation_id
        .strip_prefix(TENANT_PURGE_OPERATION_PREFIX)?
        .parse::<uuid::Uuid>()
        .ok()
        .map(TenantId::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_operation_id_round_trips_its_tenant_workflow_key() {
        // Pins: repeated edge dispatches and status polls address one stable tenant-keyed workflow.
        let tenant_id = TenantId::new();
        let operation_id = tenant_purge_operation_id(tenant_id);

        assert_eq!(
            tenant_id_from_purge_operation_id(&operation_id),
            Some(tenant_id)
        );
        assert_eq!(operation_id, tenant_purge_operation_id(tenant_id));
        assert_eq!(
            tenant_id_from_purge_operation_id(&tenant_id.to_string()),
            None
        );
    }
}
