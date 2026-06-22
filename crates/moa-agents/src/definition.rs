//! Database-facing configured-agent deployment pointers.

use moa_core::AgentRevisionLock;
use uuid::Uuid;

/// Installed-agent pointer selected for session creation or deployment resolution.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AgentInstallationPointer {
    /// Stable installed-agent row identifier.
    pub installation_uid: Uuid,
    /// Optional agent principal bound to this installation.
    pub agent_id: Option<Uuid>,
    /// Stable agent artifact row identifier.
    pub artifact_uid: Uuid,
    /// Stable agent artifact reference.
    pub definition_ref: String,
    /// User-facing configured-agent display name.
    pub display_name: String,
    /// Currently deployed agent revision.
    pub current_revision_uid: Uuid,
    /// Last active deployment row.
    pub deployment_uid: Uuid,
    /// Exact runtime policy lock selected when this installation was deployed.
    pub revision_lock: AgentRevisionLock,
}
