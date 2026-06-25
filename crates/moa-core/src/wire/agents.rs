//! Configured-agent service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for listing visible published agent definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinitionListRequest {
    /// Tenant used for authorization and artifact visibility.
    pub tenant_id: TenantId,
    /// Optional artifact status filter, defaulting to `published`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// Response payload containing tenant-configurable agent definitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionListResponse {
    /// Tenant used for artifact visibility.
    pub tenant_id: TenantId,
    /// Visible agent definitions ordered for display.
    #[serde(default)]
    pub agents: Vec<AgentDefinitionSummary>,
}

/// Summary of one visible agent artifact revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinitionSummary {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Exact revision row identifier.
    pub revision_uid: Uuid,
    /// Generated scope tier label.
    pub scope: String,
    /// Stable artifact name.
    pub name: String,
    /// Stable agent reference such as `agent://support`.
    pub definition_ref: String,
    /// Human-readable artifact description.
    pub description: String,
    /// Human-readable configured-agent display name.
    pub display_name: String,
    /// Artifact tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Revision status.
    pub status: String,
    /// Artifact-local revision version.
    pub version: i32,
    /// Timestamp when this revision was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Request payload for installing a published agent revision into a tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstallRequest {
    /// Tenant that receives the installation.
    pub tenant_id: TenantId,
    /// Exact published agent revision to install and deploy.
    pub revision_uid: Uuid,
    /// Optional agent principal bound to the installation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    /// Optional display-name override for this installation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Optional deployment reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Installation metadata owned by product/admin UI.
    #[serde(default)]
    pub metadata: Value,
}

/// Response payload returned after installing an agent revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInstallResponse {
    /// Tenant that owns the installation.
    pub tenant_id: TenantId,
    /// Stable installation pointer.
    pub installation_uid: Uuid,
    /// Stable deployment row selected by the installation.
    pub deployment_uid: Uuid,
    /// Exact published agent revision deployed.
    pub revision_uid: Uuid,
    /// Runtime policy hash selected by the deployment lock.
    pub policy_hash: String,
}

/// Request payload for listing installed agents in a tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInstallationListRequest {
    /// Tenant used for authorization and installation visibility.
    pub tenant_id: TenantId,
}

/// Response payload containing installed-agent summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstallationListResponse {
    /// Tenant used for installation visibility.
    pub tenant_id: TenantId,
    /// Installed agents ordered by latest update.
    #[serde(default)]
    pub installations: Vec<AgentInstallationSummary>,
}

/// Summary of one installed configurable agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInstallationSummary {
    /// Stable installation pointer.
    pub installation_uid: Uuid,
    /// Optional bound agent principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<Uuid>,
    /// Stable artifact row identifier.
    pub artifact_uid: Uuid,
    /// Stable agent artifact reference.
    pub definition_ref: String,
    /// Human-readable configured-agent display name.
    pub display_name: String,
    /// Installation lifecycle status.
    pub status: String,
    /// Current deployed revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision_uid: Option<Uuid>,
    /// Current deployment pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deployment_uid: Option<Uuid>,
    /// Last deployment time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_deployed_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update time.
    pub updated_at: DateTime<Utc>,
}

/// Request payload for deploying a new exact revision to an installed agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDeployRequest {
    /// Tenant that owns the installation.
    pub tenant_id: TenantId,
    /// Installed-agent pointer to move.
    pub installation_uid: Uuid,
    /// Exact published agent revision to deploy.
    pub revision_uid: Uuid,
    /// Optional deployment reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Response payload returned after deploying an agent revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDeployResponse {
    /// Tenant that owns the deployment.
    pub tenant_id: TenantId,
    /// Installed-agent pointer moved by the deployment.
    pub installation_uid: Uuid,
    /// Stable deployment row.
    pub deployment_uid: Uuid,
    /// Exact published agent revision deployed.
    pub revision_uid: Uuid,
    /// Runtime policy hash selected by the deployment lock.
    pub policy_hash: String,
}

/// Request payload for listing deployment history for an installed agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDeploymentListRequest {
    /// Tenant that owns the installation.
    pub tenant_id: TenantId,
    /// Installed-agent pointer whose history should be listed.
    pub installation_uid: Uuid,
    /// Optional maximum number of deployments to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

/// Response payload containing installed-agent deployment history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDeploymentListResponse {
    /// Tenant that owns the deployment history.
    pub tenant_id: TenantId,
    /// Installed-agent pointer whose history was listed.
    pub installation_uid: Uuid,
    /// Deployments ordered newest first.
    #[serde(default)]
    pub deployments: Vec<AgentDeploymentSummary>,
}

/// Summary of one installed-agent deployment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDeploymentSummary {
    /// Stable deployment row.
    pub deployment_uid: Uuid,
    /// Exact deployed agent revision.
    pub revision_uid: Uuid,
    /// Deployment lifecycle status.
    pub status: String,
    /// Caller who created the deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployed_by: Option<String>,
    /// Deployment reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Runtime policy hash selected by the deployment lock.
    pub dependency_lock_hash: String,
    /// Deployment creation time.
    pub deployed_at: DateTime<Utc>,
}
