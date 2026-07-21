//! Tenant knowledge connection domain types.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::InformationBarrierId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::SyncRunStatus;

/// One linked external account for one tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnection {
    /// Tenant-owned connection identifier.
    pub connection_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Linked-account provider.
    pub provider: String,
    /// Provider connector identifier.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Credential vault reference, never raw credentials.
    pub credential_ref: String,
    /// Current connection status.
    pub status: ConnectionStatus,
    /// Safe provider metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Provider-native selected source state. Empty means provider default/all.
    #[serde(default)]
    pub source_selection: Value,
    /// Source-owned information barrier applied to every record from this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub information_barrier: Option<InformationBarrierId>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last successful sync timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Connection lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Link flow has been created but not completed.
    Pending,
    /// Connection can sync records.
    Active,
    /// Connection is disabled.
    Disabled,
    /// Provider reported a recoverable or terminal error.
    Error,
}

impl ConnectionStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

/// Linked connection plus latest sync-run status for service projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionProjection {
    /// Linked connection.
    pub connection: KnowledgeConnection,
    /// Most recent sync-run status, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_status: Option<SyncRunStatus>,
}
