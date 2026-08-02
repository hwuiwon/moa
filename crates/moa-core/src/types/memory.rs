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

/// Byte length of the keyed digest inside a [`SourcePrincipalFingerprint`].
pub const SOURCE_PRINCIPAL_DIGEST_BYTES: usize = 32;

/// Byte length of the opaque encoded form of a [`SourcePrincipalFingerprint`].
///
/// Two big-endian key-version bytes followed by the keyed digest. The version is
/// part of the opaque value so a single `bytea` comparison decides both "same
/// principal" and "same ACL key generation": a fingerprint minted under a
/// retired key never matches an entry minted under the current one.
pub const SOURCE_PRINCIPAL_FINGERPRINT_BYTES: usize = 2 + SOURCE_PRINCIPAL_DIGEST_BYTES;

/// One provider ACL principal, reduced to a keyed opaque fingerprint.
///
/// A principal is a provider-native identity (a user, a group, a domain, or the
/// provider's "anyone with the link" pseudo-principal). MOA never persists the
/// underlying email, phone number, or provider label: the canonical
/// `namespace/kind/subject` triple is HMAC'd with the tenant's versioned ACL key
/// and only this fingerprint is stored, compared, logged, or placed in a cache
/// key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourcePrincipalFingerprint(
    #[serde(with = "source_principal_fingerprint_hex")] [u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES],
);

impl SourcePrincipalFingerprint {
    /// Builds a fingerprint from an ACL key version and its keyed digest.
    #[must_use]
    pub fn from_digest(key_version: u16, digest: [u8; SOURCE_PRINCIPAL_DIGEST_BYTES]) -> Self {
        let mut bytes = [0_u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES];
        bytes[..2].copy_from_slice(&key_version.to_be_bytes());
        bytes[2..].copy_from_slice(&digest);
        Self(bytes)
    }

    /// Parses the opaque encoded form read back from storage.
    pub fn from_bytes(bytes: &[u8]) -> crate::error::Result<Self> {
        <[u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES]>::try_from(bytes)
            .map(Self)
            .map_err(|_| {
                crate::error::MoaError::ValidationError(format!(
                    "source principal fingerprint must be {SOURCE_PRINCIPAL_FINGERPRINT_BYTES} bytes"
                ))
            })
    }

    /// Returns the opaque encoded form used as a database bind parameter.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the ACL key version this fingerprint was minted under.
    #[must_use]
    pub fn key_version(&self) -> u16 {
        u16::from_be_bytes([self.0[0], self.0[1]])
    }
}

mod source_principal_fingerprint_hex {
    //! Hex codec for the opaque fingerprint so serialized forms stay printable
    //! without ever exposing a decodable principal label.

    use super::SOURCE_PRINCIPAL_FINGERPRINT_BYTES;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S>(
        bytes: &[u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES],
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub(super) fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<[u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES], D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let decoded = hex::decode(&encoded).map_err(serde::de::Error::custom)?;
        <[u8; SOURCE_PRINCIPAL_FINGERPRINT_BYTES]>::try_from(decoded.as_slice()).map_err(|_| {
            serde::de::Error::custom(format!(
                "source principal fingerprint must be {SOURCE_PRINCIPAL_FINGERPRINT_BYTES} bytes"
            ))
        })
    }
}

/// The caller's resolved provider-source admission context for one request.
///
/// Built once, durably, from authenticated session/contact identity plus
/// verified provider bindings — never from request JSON and never refreshed
/// inside a retrieval leg. It carries the bounded canonical principal-set and
/// the tenant's current source-ACL epoch; the epoch is what makes a warm
/// retrieval cache entry stale the moment a snapshot or binding changes.
///
/// The default is the empty set, which denies every provider-managed source.
/// Tenant role or operator status does not widen it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceAclContext {
    principals: BTreeSet<SourcePrincipalFingerprint>,
    acl_epoch: i64,
}

impl SourceAclContext {
    /// Creates the deny-everything context at the given tenant ACL epoch.
    #[must_use]
    pub fn empty(acl_epoch: i64) -> Self {
        Self {
            principals: BTreeSet::new(),
            acl_epoch,
        }
    }

    /// Creates a context from resolved principal fingerprints and the tenant epoch.
    #[must_use]
    pub fn new(
        principals: impl IntoIterator<Item = SourcePrincipalFingerprint>,
        acl_epoch: i64,
    ) -> Self {
        Self {
            principals: principals.into_iter().collect(),
            acl_epoch,
        }
    }

    /// Returns the tenant's source-ACL epoch pinned into this context.
    #[must_use]
    pub fn acl_epoch(&self) -> i64 {
        self.acl_epoch
    }

    /// Returns the canonical sorted principal fingerprints.
    #[must_use]
    pub fn principals(&self) -> &BTreeSet<SourcePrincipalFingerprint> {
        &self.principals
    }

    /// Returns whether the caller resolved to no provider principals at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.principals.is_empty()
    }

    /// Returns the opaque fingerprints as database bind values, canonically sorted.
    #[must_use]
    pub fn bind_values(&self) -> Vec<Vec<u8>> {
        self.principals
            .iter()
            .map(|principal| principal.as_bytes().to_vec())
            .collect()
    }

    /// Returns the aggregate fingerprint of the whole principal set.
    ///
    /// This is the only ACL value permitted in a cache key: it is a digest over
    /// the already-opaque per-principal fingerprints, so two callers collide
    /// only when their admitted principal sets are byte-identical.
    #[must_use]
    pub fn principal_set_fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"moa/source-acl/principal-set/v1");
        for principal in &self.principals {
            hasher.update(&(principal.as_bytes().len() as u32).to_be_bytes());
            hasher.update(principal.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }
}

/// Request-local tenant/contact values used to install Postgres RLS GUCs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RlsContext {
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    /// Provider-source ACL admission context for this caller.
    ///
    /// Every constructor starts at [`SourceAclContext::empty`], which admits no
    /// provider-managed source; callers attach a resolved set with
    /// [`RlsContext::with_source_acl`]. There is no serde default: a serialized
    /// context without this field is a typed decode error rather than a silently
    /// widened one.
    source_acl: SourceAclContext,
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

/// Epoch value marking a source-ACL context that was never resolved.
///
/// Negative by construction so it can never equal a real tenant epoch (which
/// starts at zero and only increases). Retrieval caching treats it as
/// non-cacheable: a result computed without a resolved ACL epoch has nothing
/// that could later invalidate it.
pub const SOURCE_ACL_EPOCH_UNRESOLVED: i64 = -1;

impl RlsContext {
    /// Creates a tenant-local RLS context.
    #[must_use]
    pub fn tenant(tenant_id: TenantId) -> Self {
        Self {
            tenant_id,
            contact_id: None,
            cleared_barriers: InformationBarrierClearances::new(),
            source_acl: SourceAclContext::empty(SOURCE_ACL_EPOCH_UNRESOLVED),
        }
    }

    /// Creates a contact-local RLS context inside one tenant.
    #[must_use]
    pub fn contact(tenant_id: TenantId, contact_id: ContactId) -> Self {
        Self {
            tenant_id,
            contact_id: Some(contact_id),
            cleared_barriers: InformationBarrierClearances::new(),
            source_acl: SourceAclContext::empty(SOURCE_ACL_EPOCH_UNRESOLVED),
        }
    }

    /// Returns this context extended with the caller's resolved provider-source
    /// admission context.
    ///
    /// The unattached default admits no provider-managed source, so forgetting
    /// to attach a resolved set fails closed.
    #[must_use]
    pub fn with_source_acl(mut self, source_acl: SourceAclContext) -> Self {
        self.source_acl = source_acl;
        self
    }

    /// Returns the caller's provider-source admission context.
    #[must_use]
    pub fn source_acl(&self) -> &SourceAclContext {
        &self.source_acl
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

/// Builds the contextual embedding input for one knowledge chunk.
///
/// This is the authoritative embedding input for a `Chunk` vector, and it is
/// not the chunk text. The same sentence under a different document title or
/// heading path is a different point in the embedding space, so ingestion
/// prefixes the chunk with its context. Re-embedding from bare
/// `knowledge_chunks.text` would produce
/// vectors that look valid, index cleanly, and quietly answer from a different
/// space than the queries they are compared against.
///
/// Every chunk-embedding caller uses this function so the format cannot drift
/// between vector backends or ingestion paths.
#[must_use]
pub fn contextual_chunk_embedding_input(
    document_title: Option<&str>,
    heading_path: &[String],
    text: &str,
) -> String {
    let mut context: Vec<&str> = Vec::new();
    if let Some(title) = document_title {
        let title = title.trim();
        if !title.is_empty() {
            context.push(title);
        }
    }
    for heading in heading_path {
        let heading = heading.trim();
        if !heading.is_empty() && context.last().copied() != Some(heading) {
            context.push(heading);
        }
    }
    if context.is_empty() {
        return text.to_string();
    }
    format!("{}\n\n{}", context.join(" > "), text)
}

#[cfg(test)]
mod contextual_chunk_tests {
    use super::*;

    #[test]
    fn contextual_chunk_input_prefixes_title_and_heading_path() {
        // Pins: the authoritative Chunk embedding input is the contextual form,
        // not the bare chunk text. A rebuild that dropped the prefix would emit
        // vectors in a different space than the ones it replaces.
        let input = contextual_chunk_embedding_input(
            Some("  Security Handbook  "),
            &["Access Control".to_string(), "  ".to_string()],
            "Rotate keys quarterly.",
        );

        assert_eq!(
            input,
            "Security Handbook > Access Control\n\nRotate keys quarterly."
        );
    }

    #[test]
    fn contextual_chunk_input_collapses_a_heading_that_repeats_its_parent() {
        // Pins: the deduplication rule ingestion applies, so a rebuild of a
        // document whose first heading equals its title reproduces the exact
        // same string.
        let input = contextual_chunk_embedding_input(
            Some("Runbook"),
            &["Runbook".to_string(), "Rollback".to_string()],
            "Flip the pointer.",
        );

        assert_eq!(input, "Runbook > Rollback\n\nFlip the pointer.");
    }

    #[test]
    fn contextual_chunk_input_without_context_is_the_bare_text() {
        // Pins: an untitled, unheaded chunk embeds its text with no separator,
        // matching ingestion rather than emitting a leading "\n\n".
        assert_eq!(
            contextual_chunk_embedding_input(None, &[], "Bare chunk."),
            "Bare chunk."
        );
        assert_eq!(
            contextual_chunk_embedding_input(Some("   "), &["".to_string()], "Bare chunk."),
            "Bare chunk."
        );
    }
}
