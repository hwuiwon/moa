//! Memory-adjacent platform types.

use std::{collections::BTreeSet, fmt, iter::FromIterator, ops::Deref};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::contact::ContactId;
use super::identifiers::{StoragePartitionId, TenantId};

/// Maximum encoded length of an information-barrier identifier.
pub const MAX_INFORMATION_BARRIER_ID_BYTES: usize = 128;

/// Validated identifier for one information barrier / need-to-know domain.
///
/// The value is safe to place in the comma-delimited `moa.cleared_barriers`
/// Postgres GUC: it is nonempty, bounded, and contains neither the delimiter nor
/// control characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct InformationBarrierId(String);

impl InformationBarrierId {
    /// Parses and validates an information-barrier identifier.
    pub fn parse(value: impl Into<String>) -> crate::error::Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(crate::error::MoaError::ValidationError(
                "information barrier id must not be empty".to_string(),
            ));
        }
        if value.len() > MAX_INFORMATION_BARRIER_ID_BYTES {
            return Err(crate::error::MoaError::ValidationError(format!(
                "information barrier id exceeds {MAX_INFORMATION_BARRIER_ID_BYTES} bytes"
            )));
        }
        if value.contains(',') || value.chars().any(char::is_control) {
            return Err(crate::error::MoaError::ValidationError(
                "information barrier id must not contain commas or control characters".to_string(),
            ));
        }
        Ok(Self(value))
    }

    /// Returns the validated identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for InformationBarrierId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<String> for InformationBarrierId {
    type Error = crate::error::MoaError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for InformationBarrierId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Canonical sorted, duplicate-free information-barrier authorization context.
///
/// The policy revision participates in retrieval-cache fingerprints but is not
/// serialized with the authored clearance list. Runtime admission must attach
/// the pinned policy revision before a cacheable retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InformationBarrierClearances {
    ids: BTreeSet<InformationBarrierId>,
    policy_revision: String,
}

impl InformationBarrierClearances {
    /// Creates an empty, unversioned clearance set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches the stable revision of the policy that authorized this set.
    #[must_use]
    pub fn with_policy_revision(mut self, policy_revision: impl Into<String>) -> Self {
        self.policy_revision = policy_revision.into();
        self
    }

    /// Returns the authorization policy revision used for cache isolation.
    #[must_use]
    pub fn policy_revision(&self) -> &str {
        &self.policy_revision
    }
}

impl Default for InformationBarrierClearances {
    fn default() -> Self {
        Self {
            ids: BTreeSet::new(),
            policy_revision: "unversioned".to_string(),
        }
    }
}

impl Deref for InformationBarrierClearances {
    type Target = BTreeSet<InformationBarrierId>;

    fn deref(&self) -> &Self::Target {
        &self.ids
    }
}

impl FromIterator<InformationBarrierId> for InformationBarrierClearances {
    fn from_iter<T: IntoIterator<Item = InformationBarrierId>>(iter: T) -> Self {
        Self {
            ids: iter.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl<'a> IntoIterator for &'a InformationBarrierClearances {
    type Item = &'a InformationBarrierId;
    type IntoIter = std::collections::btree_set::Iter<'a, InformationBarrierId>;

    fn into_iter(self) -> Self::IntoIter {
        self.ids.iter()
    }
}

impl Serialize for InformationBarrierClearances {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.ids.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InformationBarrierClearances {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self {
            ids: BTreeSet::deserialize(deserializer)?,
            ..Self::default()
        })
    }
}

/// Request-local tenant/contact values used to install Postgres RLS GUCs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RlsContext {
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    /// Information-barrier / need-to-know tags this caller is cleared to retrieve.
    ///
    /// Empty means no clearance: barriered graph-memory nodes
    /// (`moa.node_index.barrier IS NOT NULL`) fail closed (hidden) under the
    /// `moa.cleared_barriers` need-to-know RLS policy, while ordinary NULL-barrier
    /// rows are unaffected. Every constructor defaults this empty so existing
    /// `RlsContext::` call sites keep the fail-closed default without change;
    /// callers opt in with [`RlsContext::with_cleared_barriers`]. `#[serde(default)]`
    /// keeps older serialized contexts (without the field) deserializing to empty.
    #[serde(default)]
    cleared_barriers: InformationBarrierClearances,
}

impl RlsContext {
    /// Creates a tenant-local RLS context.
    #[must_use]
    pub fn tenant(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            contact_id: None,
            cleared_barriers: InformationBarrierClearances::new(),
        }
    }

    /// Creates a contact-local RLS context inside one tenant.
    #[must_use]
    pub fn contact(tenant_id: TenantId, contact_id: ContactId) -> Self {
        Self {
            tenant_id,
            contact_id: Some(contact_id),
            cleared_barriers: InformationBarrierClearances::new(),
        }
    }

    /// Returns this context extended with the caller's cleared information-barrier
    /// tags (need-to-know clearances).
    ///
    /// A barriered graph-memory node is retrievable only when its tag is in this
    /// set; the empty default hides every barriered node (fail closed).
    /// NULL-barrier rows are unaffected. Builder form so the field can be added
    /// without disturbing the constructor-based call sites.
    #[must_use]
    pub fn with_cleared_barriers(mut self, cleared_barriers: InformationBarrierClearances) -> Self {
        self.cleared_barriers = cleared_barriers;
        self
    }

    /// Returns the caller's cleared information-barrier tags.
    #[must_use]
    pub fn cleared_barriers(&self) -> &InformationBarrierClearances {
        &self.cleared_barriers
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn information_barrier_id_rejects_unsafe_guc_values() {
        // Pins: barrier IDs cannot escape the comma-delimited Postgres GUC
        // encoding or introduce control bytes.
        assert!(InformationBarrierId::parse("").is_err());
        assert!(InformationBarrierId::parse("deal,alpha").is_err());
        assert!(InformationBarrierId::parse("deal\nalpha").is_err());
        assert!(
            InformationBarrierId::parse("x".repeat(MAX_INFORMATION_BARRIER_ID_BYTES + 1)).is_err()
        );
        assert_eq!(
            InformationBarrierId::parse("deal-alpha")
                .expect("valid barrier")
                .as_str(),
            "deal-alpha"
        );
    }

    #[test]
    fn information_barrier_clearances_are_canonical() {
        // Pins: the owning collection sorts and deduplicates clearances before
        // cache keys or database GUCs consume them.
        let clearances = ["zeta", "alpha", "zeta"]
            .into_iter()
            .map(|value| InformationBarrierId::parse(value).expect("valid barrier"))
            .collect::<InformationBarrierClearances>();
        assert_eq!(
            clearances
                .iter()
                .map(InformationBarrierId::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
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
    /// Whether the skill carries an optional reusable execution-plan template.
    #[serde(default)]
    pub has_execution_plan: bool,
    /// Estimated token cost for the full skill body.
    pub estimated_tokens: usize,
}
