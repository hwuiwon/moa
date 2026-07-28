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
    /// Every constructor starts at [`SourceAclContext::empty`], which admits only
    /// `TenantPublic` sources; callers attach a resolved set with
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
    /// The unattached default admits only `TenantPublic` sources, so forgetting
    /// to attach a resolved set hides provider-managed content instead of
    /// exposing it.
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

// ---------------------------------------------------------------------------
// Storage-partition index rebuilds
// ---------------------------------------------------------------------------

uuid_id!(
    /// Identifier for one durable storage-partition index rebuild operation.
    pub struct RebuildOperationId
);

uuid_id!(
    /// Identifier for one embedding generation of a storage partition.
    pub struct EmbeddingGenerationId
);

/// Which rebuild an operation performs.
///
/// Both kinds share one generation state machine: they differ in what they
/// stage and what activation replaces, not in how progress, validation,
/// activation, or rollback are sequenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildKind {
    /// Recompute every vector in the partition under a new embedding identity.
    Reembed,
    /// Recompute chunk boundaries and everything derived from them.
    Rechunk,
}

impl RebuildKind {
    /// Returns the persisted SQL discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reembed => "reembed",
            Self::Rechunk => "rechunk",
        }
    }

    /// Parses a persisted SQL discriminator.
    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "reembed" => Ok(Self::Reembed),
            "rechunk" => Ok(Self::Rechunk),
            other => Err(crate::error::MoaError::ValidationError(format!(
                "unknown rebuild kind `{other}`"
            ))),
        }
    }
}

impl fmt::Display for RebuildKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle position of one rebuild operation.
///
/// Transitions are compare-and-swap against the operation's fence token, so a
/// replayed workflow step observes a lost swap rather than reapplying a
/// transition that already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildLifecycle {
    /// Census taken, candidate generation not yet created.
    Planning,
    /// Candidate vectors are being built in bounded batches.
    Building,
    /// Bounded shadow queries are scoring the candidate generation.
    Validating,
    /// Validation passed; the candidate is complete and awaiting activation.
    AwaitingActivation,
    /// The candidate generation is the production read generation; the prior
    /// generation is retained for rollback.
    Activated,
    /// Retired generation data has been removed; the operation is closed.
    Finalized,
    /// The prior generation was restored as the production read generation.
    RolledBack,
    /// An operator cancelled the operation before activation.
    Cancelled,
    /// The operation stopped on an error and did not activate.
    Failed,
}

impl RebuildLifecycle {
    /// Returns the persisted SQL discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Building => "building",
            Self::Validating => "validating",
            Self::AwaitingActivation => "awaiting_activation",
            Self::Activated => "activated",
            Self::Finalized => "finalized",
            Self::RolledBack => "rolled_back",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    /// Parses a persisted SQL discriminator.
    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "planning" => Ok(Self::Planning),
            "building" => Ok(Self::Building),
            "validating" => Ok(Self::Validating),
            "awaiting_activation" => Ok(Self::AwaitingActivation),
            "activated" => Ok(Self::Activated),
            "finalized" => Ok(Self::Finalized),
            "rolled_back" => Ok(Self::RolledBack),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            other => Err(crate::error::MoaError::ValidationError(format!(
                "unknown rebuild lifecycle `{other}`"
            ))),
        }
    }

    /// Whether no further transition is possible.
    ///
    /// The partial unique index that admits one rebuild per storage partition
    /// uses exactly this set, so the Rust and SQL definitions cannot drift
    /// without the index rejecting a start that this predicate allowed.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Finalized | Self::RolledBack | Self::Cancelled | Self::Failed
        )
    }
}

impl fmt::Display for RebuildLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Serving state of one embedding generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationState {
    /// Built but never served. Candidate vectors live in their own table and no
    /// production reader joins it.
    Candidate,
    /// The production read generation named by the active-generation pointer.
    Active,
    /// Superseded by a later activation, retained until finalization.
    Retired,
}

impl GenerationState {
    /// Returns the persisted SQL discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Retired => "retired",
        }
    }

    /// Parses a persisted SQL discriminator.
    pub fn parse(value: &str) -> crate::error::Result<Self> {
        match value {
            "candidate" => Ok(Self::Candidate),
            "active" => Ok(Self::Active),
            "retired" => Ok(Self::Retired),
            other => Err(crate::error::MoaError::ValidationError(format!(
                "unknown generation state `{other}`"
            ))),
        }
    }
}

impl fmt::Display for GenerationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One member of the state a rechunk must stage before it can activate.
///
/// Activation replaces all of these in one scoped transaction. A rechunk that
/// staged only some of them is refused, because applying a subset would leave
/// chunks whose graph, ACL, or occurrence identity described the old text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RechunkStagingMember {
    /// Replacement chunk rows for the document version.
    Chunk,
    /// Graph node and edge deltas derived from the new chunks.
    GraphDelta,
    /// Candidate embeddings for the new chunks.
    Embedding,
    /// Source-ACL snapshot fingerprints carried forward onto the new chunks.
    AclSnapshot,
    /// Per-occurrence identity for the new chunks.
    OccurrenceIdentity,
    /// Provenance linking each new chunk to the parsed source it came from.
    Provenance,
}

impl RechunkStagingMember {
    /// Every member a complete rechunk staging set must contain.
    pub const ALL: [Self; 6] = [
        Self::Chunk,
        Self::GraphDelta,
        Self::Embedding,
        Self::AclSnapshot,
        Self::OccurrenceIdentity,
        Self::Provenance,
    ];

    /// Returns the persisted SQL discriminator.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chunk => "chunk",
            Self::GraphDelta => "graph_delta",
            Self::Embedding => "embedding",
            Self::AclSnapshot => "acl_snapshot",
            Self::OccurrenceIdentity => "occurrence_identity",
            Self::Provenance => "provenance",
        }
    }

    /// Parses a persisted SQL discriminator.
    pub fn parse(value: &str) -> crate::error::Result<Self> {
        Self::ALL
            .into_iter()
            .find(|member| member.as_str() == value)
            .ok_or_else(|| {
                crate::error::MoaError::ValidationError(format!(
                    "unknown rechunk staging member `{value}`"
                ))
            })
    }
}

impl fmt::Display for RechunkStagingMember {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Single policy governing the tenant-knowledge semantic graph.
///
/// Writes and reads are derived from ONE value on purpose. Semantic extraction
/// used to run unconditionally at ingestion while tenant-knowledge retrieval
/// hard-disabled graph expansion, so every deployment paid extraction and
/// storage cost for entities and relations no retrieval leg could read. Deriving
/// both sides from one value makes that write-only combination unrepresentable:
/// no setting writes without reading, and none reads without writing.
///
/// It lives in `moa-core` for the same reason
/// [`contextual_chunk_embedding_input`] does: the configuration crate and the
/// ingestion and retrieval crates must all agree, and a second copy of the rule
/// is a copy that drifts.
///
/// The default is [`SemanticGraphPolicy::Off`], measured on 2026-07-28 against
/// the WixQA `simulated` (200q/1000 articles) and `multihoprag` (150q/609
/// articles) corpora over a graph holding ~1,984 semantic entity nodes and
/// ~7,121 semantic edges. Graph expansion produced zero rescues and zero
/// regressions on both: the entity-consuming policy walked 1,428 and 2,908 graph
/// paths and moved no ranking position across 350 questions, at up to +64%
/// retrieval p95. See `docs/21-tenant-knowledge-base.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SemanticGraphPolicy {
    /// Semantic entities and relations are neither extracted nor written, and
    /// tenant-knowledge retrieval performs no graph expansion.
    #[default]
    Off,
    /// The deterministic keyword ruleset extracts semantic entities and
    /// relations, and tenant-knowledge retrieval expands the graph to read them.
    ///
    /// Extraction is purely lexical: no provider or LLM call is made.
    Deterministic,
}

impl SemanticGraphPolicy {
    /// Returns whether ingestion extracts and writes semantic graph data.
    #[must_use]
    pub const fn writes_semantic_graph(self) -> bool {
        matches!(self, Self::Deterministic)
    }

    /// Returns whether tenant-knowledge retrieval may expand the graph.
    ///
    /// This is the single source for that decision. Both the brain context
    /// pipeline and the orchestrator memory-search tool read it, so the two
    /// call sites cannot drift apart.
    #[must_use]
    pub const fn enables_tenant_graph_expansion(self) -> bool {
        matches!(self, Self::Deterministic)
    }

    /// Returns the stable configuration and telemetry label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Deterministic => "deterministic",
        }
    }
}

/// Builds the contextual embedding input for one knowledge chunk.
///
/// This is the authoritative embedding input for a `Chunk` vector, and it is
/// not the chunk text. The same sentence under a different document title or
/// heading path is a different point in the embedding space, so ingestion
/// prefixes the chunk with its context and a rebuild must reproduce the prefix
/// byte for byte. Re-embedding from bare `knowledge_chunks.text` would produce
/// vectors that look valid, index cleanly, and quietly answer from a different
/// space than the queries they are compared against.
///
/// Both the ingestion pipeline and the rebuild call this one function so the
/// format cannot drift between the writer and the rebuilder.
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
mod semantic_graph_policy_tests {
    use super::SemanticGraphPolicy;

    #[test]
    fn semantic_graph_writes_and_reads_are_never_independently_configurable() {
        // Pins the guarantee Task 5.5 exists to provide: there is no policy value
        // that writes the semantic graph without reading it, or reads it without
        // writing it. A write-only path is what made MOA pay extraction and storage
        // cost for data no retrieval leg could reach, and coupling both sides to one
        // value makes that combination unrepresentable rather than merely
        // discouraged.
        for policy in [SemanticGraphPolicy::Off, SemanticGraphPolicy::Deterministic] {
            assert_eq!(
                policy.writes_semantic_graph(),
                policy.enables_tenant_graph_expansion(),
                "{} must not decouple semantic writes from semantic reads",
                policy.as_str()
            );
        }
    }

    #[test]
    fn semantic_graph_defaults_to_off() {
        // Pins the measured default. Graph expansion produced zero rescues and zero
        // regressions on both the simulated and multihoprag corpora, so a default
        // deployment neither extracts semantic data nor pays to traverse it.
        let policy = SemanticGraphPolicy::default();
        assert_eq!(policy, SemanticGraphPolicy::Off);
        assert!(!policy.writes_semantic_graph());
        assert!(!policy.enables_tenant_graph_expansion());
    }

    #[test]
    fn semantic_graph_policy_labels_are_stable_kebab_case() {
        // Pins: the label is both the config token and the metric dimension, so it
        // must stay stable and match the serde representation.
        assert_eq!(SemanticGraphPolicy::Off.as_str(), "off");
        assert_eq!(SemanticGraphPolicy::Deterministic.as_str(), "deterministic");
        for policy in [SemanticGraphPolicy::Off, SemanticGraphPolicy::Deterministic] {
            let encoded = serde_json::to_string(&policy).expect("serialize policy");
            assert_eq!(encoded, format!("\"{}\"", policy.as_str()));
            let decoded: SemanticGraphPolicy =
                serde_json::from_str(&encoded).expect("round-trip policy");
            assert_eq!(decoded, policy);
        }
    }
}

#[cfg(test)]
mod rebuild_tests {
    use super::*;

    #[test]
    fn rebuild_lifecycle_terminal_set_matches_the_single_operation_index() {
        // Pins: the Rust terminal set and the V000351 partial unique index
        // predicate name the same four lifecycles. If they diverge, a second
        // rebuild either starts against a live one or is refused after a
        // finished one.
        let terminal = [
            RebuildLifecycle::Finalized,
            RebuildLifecycle::RolledBack,
            RebuildLifecycle::Cancelled,
            RebuildLifecycle::Failed,
        ];
        for lifecycle in terminal {
            assert!(
                lifecycle.is_terminal(),
                "{lifecycle} must be terminal to leave the partition free"
            );
        }
        for lifecycle in [
            RebuildLifecycle::Planning,
            RebuildLifecycle::Building,
            RebuildLifecycle::Validating,
            RebuildLifecycle::AwaitingActivation,
            RebuildLifecycle::Activated,
        ] {
            assert!(
                !lifecycle.is_terminal(),
                "{lifecycle} must hold the partition against a concurrent rebuild"
            );
        }
    }

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

    #[test]
    fn rechunk_staging_members_round_trip_their_sql_discriminators() {
        // Pins: the six-member completeness rule shares one vocabulary with
        // `moa.knowledge_rechunk_staged_members()`; a member that failed to
        // round-trip would be silently absent from the completeness check.
        for member in RechunkStagingMember::ALL {
            assert_eq!(
                RechunkStagingMember::parse(member.as_str()).expect("member round-trips"),
                member
            );
        }
        assert_eq!(RechunkStagingMember::ALL.len(), 6);
        assert!(RechunkStagingMember::parse("citations").is_err());
    }
}
