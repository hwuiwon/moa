//! Memory-owned scope types shared by retrieval, ingestion, and storage crates.

use moa_core::{
    types::contact::ContactId, types::identifiers::TenantId, types::memory::RlsContext,
};
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

/// Returns the deterministic blocking key for an extracted entity mention.
///
/// Entity ingestion and lifecycle backfill share this normalization contract so
/// punctuation and case variants resolve to the same blocking key.
#[must_use]
pub fn normalize_entity_name(name: &str) -> String {
    let mut tokens = Vec::new();
    let mut token = String::new();
    for character in name.chars() {
        if character.is_alphanumeric() {
            token.extend(character.to_lowercase());
        } else if !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }

    let normalized = tokens.join(" ");
    if normalized.is_empty() {
        name.trim().to_lowercase()
    } else {
        normalized
    }
}

/// Coarse semantic category emitted once by fact extraction.
///
/// Extraction is the single place that decides a fact's category; downstream
/// consumers (for example digest ordering) read this structured value instead
/// of re-deriving the kind from predicate prose. Keeping the decision at
/// extraction time means it generalizes with the extraction prompt rather than
/// through hardcoded keyword lists tuned to a fixed corpus. Absent or
/// unrecognized values deserialize to [`FactCategory::Other`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactCategory {
    /// A standing user or tenant preference.
    Preference,
    /// A durable identity attribute of the subject.
    Identity,
    /// A relationship between entities (ownership, dependency, membership).
    Relationship,
    /// A time-bound occurrence.
    Event,
    /// Anything extraction could not confidently categorize; the conservative
    /// default.
    #[default]
    Other,
}

/// Semantic graph edge label emitted by extraction for a fact's object
/// relationship.
///
/// Extraction chooses the relationship once; slow-path ingestion maps it onto
/// the storage edge label without re-parsing predicate text. This deliberately
/// exposes only the object-relationship labels extraction is allowed to assign;
/// absent or unrecognized values deserialize to the generic
/// [`FactEdgeLabel::RelatesTo`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactEdgeLabel {
    /// Dependency relationship (the subject depends on, uses, requires, calls,
    /// or is built on the object).
    DependsOn,
    /// Ownership or stewardship relationship (the object owns, maintains, or is
    /// responsible for the subject).
    OwnedBy,
    /// Generic semantic relationship; the conservative default.
    #[default]
    RelatesTo,
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

#[cfg(test)]
mod tests {
    use super::{FactCategory, FactEdgeLabel, normalize_entity_name};

    #[test]
    fn fact_category_wire_form_and_defaults_are_stable() {
        // Pins: the category persisted in node properties round-trips through the
        // exact snake_case strings downstream reads, and absent/unknown values
        // deserialize to Other so a missing field never mis-orders digests.
        assert_eq!(
            serde_json::to_value(FactCategory::Preference).unwrap(),
            serde_json::json!("preference")
        );
        assert_eq!(FactCategory::default(), FactCategory::Other);
        assert_eq!(
            serde_json::from_value::<FactCategory>(serde_json::json!("relationship")).unwrap(),
            FactCategory::Relationship
        );
        assert!(serde_json::from_value::<FactCategory>(serde_json::json!("bogus")).is_err());
    }

    #[test]
    fn fact_edge_label_wire_form_and_default_are_stable() {
        // Pins: extraction edge labels serialize to the strings slow-path stores
        // and the default is the conservative generic relationship.
        assert_eq!(
            serde_json::to_value(FactEdgeLabel::DependsOn).unwrap(),
            serde_json::json!("depends_on")
        );
        assert_eq!(
            serde_json::to_value(FactEdgeLabel::OwnedBy).unwrap(),
            serde_json::json!("owned_by")
        );
        assert_eq!(FactEdgeLabel::default(), FactEdgeLabel::RelatesTo);
    }

    #[test]
    fn entity_name_normalization_blocks_case_and_punctuation_variants() {
        // Pins: every memory crate treats case and punctuation variants as the
        // same entity blocking key through the shared types-owned helper.
        assert_eq!(normalize_entity_name(" API_Service "), "api service");
        assert_eq!(normalize_entity_name("api-service"), "api service");
        assert_eq!(normalize_entity_name("api.service"), "api service");
    }

    #[test]
    fn entity_name_normalization_preserves_unicode_alphanumerics() {
        // Pins: non-ASCII entity names retain their letters while separators
        // collapse, instead of silently producing an empty blocking key.
        assert_eq!(normalize_entity_name("  Café—服务  "), "café 服务");
        assert_eq!(normalize_entity_name("---"), "---");
    }
}
