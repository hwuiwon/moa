//! Memory-owned scope types shared by retrieval, ingestion, and storage crates.

use moa_core::{ContactId, RlsContext, TenantId};
use serde::{Deserialize, Serialize};

/// Normalizes one fact-content component (subject, predicate, or object) for
/// identity comparison.
///
/// This is the shared semantic contract behind fact-content identity: final
/// retrieval selection, consolidation duplicate merging, and eval probe
/// equivalence all treat facts with equal normalized `(subject, predicate,
/// object)` as the same content. Keep every comparer on this helper so the
/// three stay in agreement.
#[must_use]
pub fn normalize_fact_component(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Runtime graph-memory scope.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryScope {
    /// Tenant-local memory that is not attached to an individual contact.
    Tenant {
        /// Tenant owning this memory scope.
        tenant_id: TenantId,
    },
    /// Contact-local memory inside one tenant.
    Contact {
        /// Tenant owning the contact.
        tenant_id: TenantId,
        /// Contact owning this memory scope.
        contact_id: ContactId,
    },
}

/// Fast discriminator for runtime memory scope tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeTier {
    /// Tenant-local memory tier.
    Tenant,
    /// Contact-local memory tier.
    Contact,
}

impl MemoryScope {
    /// Returns the tenant identifier for this scope.
    pub fn tenant_id(&self) -> TenantId {
        match self {
            MemoryScope::Tenant { tenant_id } | MemoryScope::Contact { tenant_id, .. } => {
                *tenant_id
            }
        }
    }

    /// Returns the contact identifier for contact-local memory.
    pub fn contact_id(&self) -> Option<ContactId> {
        match self {
            MemoryScope::Contact { contact_id, .. } => Some(*contact_id),
            MemoryScope::Tenant { .. } => None,
        }
    }

    /// Returns whether this scope is contact-local.
    pub fn is_contact(&self) -> bool {
        matches!(self, MemoryScope::Contact { .. })
    }

    /// Returns the tier discriminator for this memory scope.
    pub fn tier(&self) -> ScopeTier {
        match self {
            MemoryScope::Tenant { .. } => ScopeTier::Tenant,
            MemoryScope::Contact { .. } => ScopeTier::Contact,
        }
    }
}

impl MemoryScope {
    /// Converts this memory-specific scope into the platform RLS context.
    #[must_use]
    pub fn to_rls_context(&self) -> RlsContext {
        match self {
            MemoryScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
            MemoryScope::Contact {
                tenant_id,
                contact_id,
            } => RlsContext::contact(*tenant_id, *contact_id),
        }
    }
}

impl From<&MemoryScope> for RlsContext {
    fn from(scope: &MemoryScope) -> Self {
        scope.to_rls_context()
    }
}

impl From<MemoryScope> for RlsContext {
    fn from(scope: MemoryScope) -> Self {
        scope.to_rls_context()
    }
}
