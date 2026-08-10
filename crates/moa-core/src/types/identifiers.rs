//! Identifier newtypes shared across MOA crates.

uuid_id!(
    /// Identifier for a MOA session.
    pub struct SessionId
);
uuid_id!(
    /// Identifier for one task segment within a session.
    pub struct SegmentId
);
uuid_id!(
    /// Identifier for a durable attachment linked from a session message.
    pub struct SessionAttachmentId
);
string_id!(
    /// Identifier for a MOA user.
    pub struct UserId
);
string_id!(
    /// Identifier for a persisted storage partition.
    pub struct StoragePartitionId
);
uuid_id!(
    /// Identifier for a tenant runtime boundary.
    pub struct TenantId
);
uuid_id!(
    /// Identifier for one tenant-installed connector connection.
    pub struct ConnectorConnectionId
);

impl StoragePartitionId {
    /// Returns the default storage partition for tenant-scoped runtime state.
    #[must_use]
    pub fn for_tenant(tenant_id: TenantId) -> Self {
        Self::new(tenant_id.to_string())
    }
}

impl From<uuid::Uuid> for TenantId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}
uuid_id!(
    /// Identifier for a brain execution instance.
    pub struct BrainId
);
string_id!(
    /// Stable identifier for an LLM model (e.g., "gpt-5.4", "claude-sonnet-4-6").
    pub struct ModelId
);

impl Default for ModelId {
    fn default() -> Self {
        Self::new("")
    }
}

uuid_id!(
    /// Stable identifier for a single tool call within a session.
    pub struct ToolCallId
);

uuid_id!(
    /// Durable identity for one hand-provisioning operation.
    pub struct HandProvisioningOperationId
);

uuid_id!(
    /// Identifier for one tenant-owned durable sandbox workspace.
    pub struct SandboxWorkspaceId
);

uuid_id!(
    /// Identifier for one immutable sandbox-workspace checkpoint.
    pub struct WorkspaceCheckpointId
);

uuid_id!(
    /// Identifier for one durable sandbox-workspace provider operation.
    pub struct WorkspaceOperationId
);

uuid_id!(
    /// Identifier for one configured sandbox provider account and isolation cell.
    pub struct ProviderAccountId
);

uuid_id!(
    /// Core boundary reference to one verified durable execution run.
    ///
    /// The owning `moa-execution` crate retains its execution-run domain type.
    /// Orchestration converts that verified UUID at the workspace boundary.
    pub struct ExecutionRunScopeId
);

uuid_id!(
    /// Core boundary reference to one verified durable execution task.
    ///
    /// The owning `moa-execution` crate retains its `ExecutionTaskId` domain
    /// type. Orchestration converts that verified UUID at the workspace
    /// boundary without introducing a `moa-core -> moa-execution` dependency.
    pub struct ExecutionTaskScopeId
);

uuid_id!(
    /// Identifier for one durable child-to-parent attention signal.
    pub struct AgentSignalId
);

impl From<uuid::Uuid> for ToolCallId {
    fn from(value: uuid::Uuid) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectorConnectionId, ExecutionRunScopeId, ExecutionTaskScopeId, ProviderAccountId,
        SandboxWorkspaceId, StoragePartitionId, TenantId, WorkspaceCheckpointId,
        WorkspaceOperationId,
    };

    #[test]
    fn storage_partition_id_for_tenant_uses_tenant_uuid_text() {
        // Pins: tenant-scoped runtime state uses the tenant UUID text as its storage partition.
        let tenant_uuid = uuid::Uuid::parse_str("018f8f1f-36a6-7c90-a7f8-2f2f57f5c111")
            .expect("fixture tenant UUID should parse");
        let tenant_id = TenantId::from(tenant_uuid);

        assert_eq!(
            StoragePartitionId::for_tenant(tenant_id).as_str(),
            tenant_uuid.to_string()
        );
    }

    #[test]
    fn connector_connection_id_round_trips_as_uuid_json() {
        // Pins: connector, knowledge, hand, wire, and orchestration boundaries
        // exchange one UUID identity rather than unrelated string aliases.
        let connection_id = ConnectorConnectionId(uuid::Uuid::from_u128(0x0c01_1ec7));

        let encoded = serde_json::to_string(&connection_id)
            .expect("connector connection id should serialize");
        let decoded: ConnectorConnectionId =
            serde_json::from_str(&encoded).expect("connector connection id should deserialize");

        assert_eq!(decoded, connection_id);
        assert_eq!(encoded, format!("\"{}\"", connection_id.0));
    }

    #[test]
    fn sandbox_workspace_identifiers_round_trip_as_distinct_uuid_newtypes() {
        // Pins: workspace, checkpoint, operation, provider-account, and durable
        // execution-task boundaries remain UUID-shaped without string aliases.
        let value = uuid::Uuid::from_u128(0x5a11_db0c);
        let encoded = serde_json::to_string(&(
            SandboxWorkspaceId(value),
            WorkspaceCheckpointId(value),
            WorkspaceOperationId(value),
            ProviderAccountId(value),
            ExecutionRunScopeId(value),
            ExecutionTaskScopeId(value),
        ))
        .expect("workspace identifiers should serialize");
        let decoded: (
            SandboxWorkspaceId,
            WorkspaceCheckpointId,
            WorkspaceOperationId,
            ProviderAccountId,
            ExecutionRunScopeId,
            ExecutionTaskScopeId,
        ) = serde_json::from_str(&encoded).expect("workspace identifiers should deserialize");

        assert_eq!(
            decoded,
            (
                SandboxWorkspaceId(value),
                WorkspaceCheckpointId(value),
                WorkspaceOperationId(value),
                ProviderAccountId(value),
                ExecutionRunScopeId(value),
                ExecutionTaskScopeId(value),
            )
        );
    }
}
