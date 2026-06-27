//! Memory-adjacent platform types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::contact::ContactId;
use super::identifiers::{StoragePartitionId, TenantId};

/// Request-local tenant/contact values used to install Postgres RLS GUCs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RlsContext {
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
}

impl RlsContext {
    /// Creates a tenant-local RLS context.
    #[must_use]
    pub fn tenant(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            contact_id: None,
        }
    }

    /// Creates a contact-local RLS context inside one tenant.
    #[must_use]
    pub fn contact(tenant_id: TenantId, contact_id: ContactId) -> Self {
        Self {
            tenant_id,
            contact_id: Some(contact_id),
        }
    }

    /// Returns the tenant identifier for this context.
    #[must_use]
    pub fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Returns the storage partition identifier implied by this context.
    #[must_use]
    pub fn storage_partition_id(&self) -> StoragePartitionId {
        StoragePartitionId::for_tenant(self.tenant_id)
    }

    /// Returns the contact identifier for contact-local data.
    #[must_use]
    pub fn contact_id(&self) -> Option<ContactId> {
        self.contact_id
    }

    /// Returns the canonical SQL value for the scope tier.
    #[must_use]
    pub fn tier_str(&self) -> &'static str {
        if self.contact_id.is_some() {
            "contact"
        } else {
            "tenant"
        }
    }
}

/// Tier-1 skill metadata injected into the context pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMetadata {
    /// Exact artifact revision backing this skill metadata, when loaded from artifacts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_revision_uid: Option<uuid::Uuid>,
    /// Canonical skill document path.
    pub path: String,
    /// Stable skill name from `SKILL.md`.
    pub name: String,
    /// Longer description from the Agent Skills frontmatter.
    pub description: String,
    /// User-defined tags.
    pub tags: Vec<String>,
    /// Tools referenced by the skill.
    pub allowed_tools: Vec<String>,
    /// Callable action names exposed by the skill artifact, if any.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
    /// Historical usage count.
    pub use_count: u32,
    /// Last time the skill was used, when tracked in metadata.
    pub last_used: Option<DateTime<Utc>>,
    /// Historical success rate between `0.0` and `1.0`.
    pub success_rate: f32,
    /// Whether the skill was auto-generated.
    pub auto_generated: bool,
}
