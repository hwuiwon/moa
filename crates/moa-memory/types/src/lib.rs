//! Memory-owned scope types shared by retrieval, ingestion, and storage crates.

use moa_core::{ContactId, TenantId};
use serde::{Deserialize, Serialize};

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
    /// Returns the retrieval scopes to search for this runtime memory scope.
    pub fn ancestors(&self) -> Vec<MemoryScope> {
        match self {
            MemoryScope::Tenant { tenant_id } => vec![MemoryScope::Tenant {
                tenant_id: *tenant_id,
            }],
            MemoryScope::Contact {
                tenant_id,
                contact_id,
            } => vec![MemoryScope::Contact {
                tenant_id: *tenant_id,
                contact_id: *contact_id,
            }],
        }
    }

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

/// Request-local scope values used to install Postgres RLS GUCs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScopeContext {
    scope: MemoryScope,
}

impl ScopeContext {
    /// Creates a scope context from a concrete memory scope.
    pub fn new(scope: MemoryScope) -> Self {
        Self { scope }
    }

    /// Creates a tenant-local scope context.
    pub fn tenant(tenant_id: TenantId) -> Self {
        Self::new(MemoryScope::Tenant { tenant_id })
    }

    /// Creates a contact-local scope context.
    pub fn contact(tenant_id: TenantId, contact_id: ContactId) -> Self {
        Self::new(MemoryScope::Contact {
            tenant_id,
            contact_id,
        })
    }

    /// Returns the concrete memory scope for this context.
    pub fn scope(&self) -> &MemoryScope {
        &self.scope
    }

    /// Returns the tenant identifier for this context.
    pub fn tenant_id(&self) -> TenantId {
        self.scope.tenant_id()
    }

    /// Returns the contact identifier for contact-local memory.
    pub fn contact_id(&self) -> Option<ContactId> {
        self.scope.contact_id()
    }

    /// Returns the tier discriminator for this context.
    pub fn tier(&self) -> ScopeTier {
        self.scope.tier()
    }

    /// Returns the canonical SQL value for the scope tier.
    pub fn tier_str(&self) -> &'static str {
        match self.scope.tier() {
            ScopeTier::Tenant => "tenant",
            ScopeTier::Contact => "contact",
        }
    }
}

impl From<MemoryScope> for ScopeContext {
    fn from(scope: MemoryScope) -> Self {
        Self::new(scope)
    }
}
