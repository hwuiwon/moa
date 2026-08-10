//! Secret-free wire contracts for tenant sandbox-workspace management.

use chrono::{DateTime, Utc};
use moa_core::types::{
    identifiers::{SandboxWorkspaceId, WorkspaceCheckpointId},
    sandbox_workspace::{DurabilityClass, SandboxWorkspaceScope, SandboxWorkspaceState},
};
use serde::{Deserialize, Serialize};

/// Request to create one workspace for a verified durable execution owner.
///
/// Tenant and provider selection are intentionally absent. The service derives
/// the tenant from authenticated identity and selects storage through governed
/// capability admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSandboxWorkspaceRequest {
    /// Worker or execution-task owner of the durable filesystem state.
    pub scope: SandboxWorkspaceScope,
    /// Required provider-independent durability behavior.
    pub durability_class: DurabilityClass,
    /// Optional deadline after which retention cleanup may begin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_deadline_at: Option<DateTime<Utc>>,
}

/// Empty request for listing workspaces visible to the authenticated caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceListRequest {}

/// Logical workspace selector used by get and lifecycle handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceIdRequest {
    /// Durable logical workspace selected by the public route.
    pub workspace_id: SandboxWorkspaceId,
}

/// Public body for restoring one exact committed checkpoint.
///
/// The workspace identity is intentionally absent and is derived from the
/// request path by the edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceRestoreBody {
    /// Logical immutable checkpoint to restore.
    pub checkpoint_id: WorkspaceCheckpointId,
}

/// Internal command for restoring one exact committed checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreSandboxWorkspaceRequest {
    /// Durable logical workspace selected by the public route.
    pub workspace_id: SandboxWorkspaceId,
    /// Logical immutable checkpoint to restore.
    pub checkpoint_id: WorkspaceCheckpointId,
}

/// Public, provider-neutral view of one sandbox workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceSummary {
    /// Durable logical workspace identity.
    pub workspace_id: SandboxWorkspaceId,
    /// Worker or execution-task owner of the workspace.
    pub scope: SandboxWorkspaceScope,
    /// Required provider-independent durability behavior.
    pub durability_class: DurabilityClass,
    /// Current durable lifecycle state.
    pub state: SandboxWorkspaceState,
    /// Current logical single-writer fence.
    pub writer_epoch: u64,
    /// Current logical compute-instance fence.
    pub instance_generation: u64,
    /// Current committed checkpoint generation.
    pub checkpoint_generation: u64,
    /// Current committed logical checkpoint, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<WorkspaceCheckpointId>,
    /// Optional deadline after which retention cleanup may begin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_deadline_at: Option<DateTime<Utc>>,
    /// Whether local lifecycle policy has fenced all caller access.
    pub access_fenced: bool,
}

/// Public list of already-authorized sandbox workspaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxWorkspaceListResponse {
    /// Workspaces visible to the authenticated caller.
    pub workspaces: Vec<SandboxWorkspaceSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::identifiers::SessionId;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn sandbox_workspace_summary_serializes_only_logical_public_state() {
        // Pins: public workspace responses cannot grow provider identifiers,
        // paths, object keys, credentials, tokens, or file-content carriers.
        let summary = SandboxWorkspaceSummary {
            workspace_id: SandboxWorkspaceId(Uuid::from_u128(1)),
            scope: SandboxWorkspaceScope::Worker {
                session_id: SessionId(Uuid::from_u128(2)),
                worker_id: Uuid::from_u128(3).to_string(),
            },
            durability_class: DurabilityClass::PortableFilesystem,
            state: SandboxWorkspaceState::Active,
            writer_epoch: 4,
            instance_generation: 5,
            checkpoint_generation: 6,
            checkpoint_id: Some(WorkspaceCheckpointId(Uuid::from_u128(7))),
            retention_deadline_at: None,
            access_fenced: false,
        };

        assert_eq!(
            serde_json::to_value(summary).expect("workspace summary should serialize"),
            json!({
                "workspace_id": "00000000-0000-0000-0000-000000000001",
                "scope": {
                    "kind": "worker",
                    "session_id": "00000000-0000-0000-0000-000000000002",
                    "worker_id": "00000000-0000-0000-0000-000000000003"
                },
                "durability_class": "portable_filesystem",
                "state": "active",
                "writer_epoch": 4,
                "instance_generation": 5,
                "checkpoint_generation": 6,
                "checkpoint_id": "00000000-0000-0000-0000-000000000007",
                "access_fenced": false
            })
        );
    }
}
