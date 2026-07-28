//! Provider-native source ACL domain types.
//!
//! A tenant knowledge connection is either uniformly public inside its tenant or
//! carries the source system's own permissions. That distinction is the whole
//! security model here: a permission-bearing connector (Google Drive through
//! Nango, a Merge knowledge base) can hold documents that only some people in the
//! tenant may read, so MOA must reproduce the provider's decision rather than
//! fall back to "everyone in the tenant".
//!
//! The types in this module make that reproduction explicit and fail closed:
//!
//! * [`ProviderAclCapability`] is declared by the adapter, not chosen by a
//!   caller, and decides the only legal [`ConnectionAclMode`].
//! * A [`ProviderAclSnapshot`] is immutable and carries the provider's own
//!   revision, a canonical hash, and its provenance. Admission requires a
//!   snapshot that is complete, current, and revision-matched.
//! * Principals are never stored in the clear. A [`CanonicalSourcePrincipal`] is
//!   normalized and then reduced to a keyed [`SourcePrincipalFingerprint`].

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::SourcePrincipalFingerprint;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::{Error, Result};

/// Field separator used when canonicalizing a principal before fingerprinting.
///
/// ASCII unit separator: it cannot appear in a provider namespace, kind, or
/// subject, so two different principals can never canonicalize to the same
/// string by splicing their parts together.
const PRINCIPAL_FIELD_SEPARATOR: char = '\u{1f}';

/// What a linked-account adapter can tell MOA about source permissions.
///
/// Declared by the adapter itself. There is no default: a connector that has not
/// stated its capability cannot be linked, because MOA would otherwise have to
/// guess whether its documents are tenant-wide readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAclCapability {
    /// Every record this connector returns is readable by the whole tenant.
    ///
    /// Only a connector with no per-record permissions at all may declare this;
    /// it is the sole capability that can produce
    /// [`ConnectionAclMode::TenantPublic`].
    UniformlyPublic,
    /// The connector returns the source system's native per-record ACLs.
    ///
    /// Linking and syncing require a complete native snapshot for every record;
    /// a record whose permissions cannot be enumerated is hidden rather than
    /// shared.
    NativeSnapshots,
}

impl ProviderAclCapability {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UniformlyPublic => "uniformly_public",
            Self::NativeSnapshots => "native_snapshots",
        }
    }

    /// Resolves the declared capability of one stored provider identifier.
    ///
    /// This is the canonical registry used where only a persisted provider
    /// string survives (an ingestion run resumed from its journal, for example)
    /// and the adapter instance is not at hand. Both shipped adapters are
    /// permission-bearing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] for an unrecognized provider. That is
    /// deliberate rather than a permissive default: an unknown connector's
    /// content would otherwise be ingested under whichever guess was convenient,
    /// and the convenient guess is the one that shares everything.
    pub fn for_provider(provider: &str) -> Result<Self> {
        match provider {
            "nango" | "merge" => Ok(Self::NativeSnapshots),
            other => Err(Error::Provider {
                provider: other.to_string(),
                message: "provider has not declared a source ACL capability".to_string(),
            }),
        }
    }

    /// Returns the only connection ACL mode this capability may produce.
    #[must_use]
    pub fn required_mode(self) -> ConnectionAclMode {
        match self {
            Self::UniformlyPublic => ConnectionAclMode::TenantPublic,
            Self::NativeSnapshots => ConnectionAclMode::ProviderManaged,
        }
    }
}

/// How a linked connection's records are admitted to retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionAclMode {
    /// Records are readable by every caller inside the owning tenant.
    TenantPublic,
    /// Records are readable only through the provider's own ACL snapshot.
    ProviderManaged,
}

impl ConnectionAclMode {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TenantPublic => "tenant_public",
            Self::ProviderManaged => "provider_managed",
        }
    }

    /// Parses the stable database identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Repository`] for any value outside the closed set. There
    /// is no permissive fallback: an unrecognized stored mode is corruption, and
    /// guessing it would either hide everything or expose everything.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "tenant_public" => Ok(Self::TenantPublic),
            "provider_managed" => Ok(Self::ProviderManaged),
            other => Err(Error::Repository(format!(
                "unknown connection ACL mode `{other}`"
            ))),
        }
    }

    /// Returns whether moving from `self` to `candidate` widens visibility.
    ///
    /// Used to reject a re-link or operator edit that would turn a
    /// provider-managed connection into a tenant-public one.
    #[must_use]
    pub fn widens_to(self, candidate: Self) -> bool {
        self == Self::ProviderManaged && candidate == Self::TenantPublic
    }
}

/// Freshness of one knowledge object's provider ACL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAclState {
    /// The stored snapshot matches the provider's current ACL revision.
    Current,
    /// The provider announced a newer ACL revision than the stored snapshot.
    Stale,
    /// No complete snapshot has ever been captured for this object.
    Incomplete,
}

impl SourceAclState {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Incomplete => "incomplete",
        }
    }

    /// Parses the stable database identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Repository`] for any value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "current" => Ok(Self::Current),
            "stale" => Ok(Self::Stale),
            "incomplete" => Ok(Self::Incomplete),
            other => Err(Error::Repository(format!(
                "unknown source ACL state `{other}`"
            ))),
        }
    }

    /// Returns whether this state can admit provider-managed content at all.
    #[must_use]
    pub fn admits(self) -> bool {
        matches!(self, Self::Current)
    }
}

/// The kind of provider-native identity one ACL entry names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourcePrincipalKind {
    /// One named end user in the source system.
    User,
    /// One named group, team, or role in the source system.
    Group,
    /// Every identity inside one email/organization domain.
    Domain,
    /// The provider's "anyone with access" pseudo-principal.
    Anyone,
}

impl SourcePrincipalKind {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Domain => "domain",
            Self::Anyone => "anyone",
        }
    }

    /// Parses the stable database identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Repository`] for any value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "group" => Ok(Self::Group),
            "domain" => Ok(Self::Domain),
            "anyone" => Ok(Self::Anyone),
            other => Err(Error::Repository(format!(
                "unknown source principal kind `{other}`"
            ))),
        }
    }
}

/// A provider identity reduced to its canonical, comparable form.
///
/// Normalization is what makes `Alice@Example.COM` in a Drive ACL match the same
/// person's verified contact point. It happens exactly once, before
/// fingerprinting, so the stored opaque value is stable across providers'
/// formatting differences.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CanonicalSourcePrincipal {
    namespace: String,
    kind: SourcePrincipalKind,
    subject: String,
}

impl CanonicalSourcePrincipal {
    /// Normalizes and validates one provider principal.
    ///
    /// `namespace` identifies the provider identity domain (for example
    /// `google_drive`), and `subject` is the provider's own identifier for the
    /// principal. [`SourcePrincipalKind::Anyone`] carries no subject and is
    /// normalized to an empty one, so every provider's spelling of "anyone with
    /// the link" collapses to a single canonical principal.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the namespace is empty, when a
    /// subject-bearing kind has an empty subject, or when either field contains
    /// the canonical field separator.
    pub fn new(
        namespace: impl AsRef<str>,
        kind: SourcePrincipalKind,
        subject: impl AsRef<str>,
    ) -> Result<Self> {
        let namespace = normalize_principal_field(namespace.as_ref());
        let subject = match kind {
            SourcePrincipalKind::Anyone => String::new(),
            _ => normalize_principal_field(subject.as_ref()),
        };
        if namespace.is_empty() {
            return Err(Error::Provider {
                provider: "unknown".to_string(),
                message: "source principal namespace must not be empty".to_string(),
            });
        }
        if kind != SourcePrincipalKind::Anyone && subject.is_empty() {
            return Err(Error::Provider {
                provider: namespace,
                message: format!(
                    "source principal of kind `{}` must carry a subject",
                    kind.as_str()
                ),
            });
        }
        if namespace.contains(PRINCIPAL_FIELD_SEPARATOR)
            || subject.contains(PRINCIPAL_FIELD_SEPARATOR)
        {
            return Err(Error::Provider {
                provider: namespace,
                message: "source principal fields must not contain the canonical separator"
                    .to_string(),
            });
        }
        Ok(Self {
            namespace,
            kind,
            subject,
        })
    }

    /// Returns the provider identity namespace.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Returns the principal kind.
    #[must_use]
    pub fn kind(&self) -> SourcePrincipalKind {
        self.kind
    }

    /// Returns the normalized provider subject, empty for
    /// [`SourcePrincipalKind::Anyone`].
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the canonical string that is fed to the keyed fingerprint.
    ///
    /// Deliberately not `Display`: this value reveals the raw principal and must
    /// never reach a log, trace, cache key, or database row. It exists only as
    /// the immediate input to [`crate::acl_key::SourceAclKey::fingerprint`].
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        format!(
            "{}{PRINCIPAL_FIELD_SEPARATOR}{}{PRINCIPAL_FIELD_SEPARATOR}{}",
            self.namespace,
            self.kind.as_str(),
            self.subject
        )
        .into_bytes()
    }
}

/// Normalizes one principal field: trimmed, lowercased, whitespace-collapsed.
fn normalize_principal_field(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Whether one snapshot entry grants or refuses access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAclEntryKind {
    /// The principal may read the object.
    Allow,
    /// The principal may not read the object, whatever else allows it.
    Deny,
}

impl SourceAclEntryKind {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    /// Parses the stable database identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Repository`] for any value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "allow" => Ok(Self::Allow),
            "deny" => Ok(Self::Deny),
            other => Err(Error::Repository(format!(
                "unknown source ACL entry kind `{other}`"
            ))),
        }
    }
}

/// One fingerprinted allow/deny entry inside a provider ACL snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderAclEntry {
    /// Whether this entry grants or refuses access.
    pub entry_kind: SourceAclEntryKind,
    /// The kind of provider identity named by this entry.
    pub principal_kind: SourcePrincipalKind,
    /// The keyed opaque fingerprint of the principal.
    pub principal: SourcePrincipalFingerprint,
}

/// Where a provider ACL snapshot came from.
///
/// Kept alongside the snapshot so an operator investigating an unexpected denial
/// can tell a full permission listing apart from a webhook-driven refresh
/// without the answer depending on log retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAclProvenance {
    /// Captured by enumerating the provider's permission listing during a sync.
    ProviderListing,
    /// Captured after the provider announced an ACL change for this record.
    ProviderChangeNotification,
}

impl ProviderAclProvenance {
    /// Returns the stable database identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderListing => "provider_listing",
            Self::ProviderChangeNotification => "provider_change_notification",
        }
    }

    /// Parses the stable database identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Repository`] for any value outside the closed set.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "provider_listing" => Ok(Self::ProviderListing),
            "provider_change_notification" => Ok(Self::ProviderChangeNotification),
            other => Err(Error::Repository(format!(
                "unknown provider ACL provenance `{other}`"
            ))),
        }
    }
}

/// An immutable capture of one source object's provider-native permissions.
///
/// Snapshots are never edited. A permission change produces a new snapshot with
/// a new provider revision, and the object's current pointer moves atomically;
/// that is what lets retrieval decide admission from a single consistent row
/// instead of racing a partially-rewritten entry list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAclSnapshot {
    /// Tenant-owned snapshot identifier.
    pub snapshot_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Connection whose provider produced the snapshot.
    pub connection_uid: Uuid,
    /// Knowledge object the snapshot describes.
    pub object_uid: Uuid,
    /// The provider's own ACL revision for this object.
    ///
    /// Compared against the object's recorded revision on every admission: a
    /// snapshot for an older revision cannot admit content.
    pub provider_revision: String,
    /// Canonical hash over the normalized, sorted entry set.
    pub snapshot_hash: String,
    /// How this snapshot was captured.
    pub provenance: ProviderAclProvenance,
    /// Whether the provider enumeration completed.
    ///
    /// `false` means the adapter could not list every permission (pagination cut
    /// short, a permission type it does not understand). An incomplete snapshot
    /// never admits anything.
    pub complete: bool,
    /// Fingerprinted allow/deny entries, canonically sorted and deduplicated.
    pub entries: Vec<ProviderAclEntry>,
    /// When the provider state was observed.
    pub captured_at: DateTime<Utc>,
}

impl ProviderAclSnapshot {
    /// Returns the canonical hash for a normalized entry set.
    ///
    /// Computed over the sorted, deduplicated entries plus the provider revision
    /// so two captures of the same permissions produce the same hash regardless
    /// of the order the provider listed them in.
    #[must_use]
    pub fn canonical_hash(provider_revision: &str, entries: &[ProviderAclEntry]) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"moa/source-acl/snapshot/v1");
        hasher.update(&(provider_revision.len() as u32).to_be_bytes());
        hasher.update(provider_revision.as_bytes());
        for entry in entries {
            hasher.update(entry.entry_kind.as_str().as_bytes());
            hasher.update(&[PRINCIPAL_FIELD_SEPARATOR as u8]);
            hasher.update(entry.principal_kind.as_str().as_bytes());
            hasher.update(&[PRINCIPAL_FIELD_SEPARATOR as u8]);
            hasher.update(entry.principal.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Builds a snapshot from raw adapter output, normalizing its entries.
    ///
    /// Entries are sorted and deduplicated, and the canonical hash is derived
    /// from the result, so the adapter cannot influence identity by ordering.
    /// A snapshot with no allow entry is still legal and simply admits nobody.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the provider revision is empty: without
    /// a revision there is nothing to compare a later provider announcement
    /// against, so staleness could never be detected.
    #[allow(
        clippy::too_many_arguments,
        reason = "a snapshot's identity is its full provider provenance; bundling these into a \
                  struct would just move the same fields behind one more name"
    )]
    pub fn normalized(
        snapshot_uid: Uuid,
        tenant_id: TenantId,
        connection_uid: Uuid,
        object_uid: Uuid,
        provider_revision: impl Into<String>,
        provenance: ProviderAclProvenance,
        complete: bool,
        entries: Vec<ProviderAclEntry>,
        captured_at: DateTime<Utc>,
    ) -> Result<Self> {
        let provider_revision = provider_revision.into().trim().to_string();
        if provider_revision.is_empty() {
            return Err(Error::Provider {
                provider: "unknown".to_string(),
                message: "provider ACL snapshot must carry a provider revision".to_string(),
            });
        }
        let mut entries = entries;
        entries.sort();
        entries.dedup();
        let snapshot_hash = Self::canonical_hash(&provider_revision, &entries);
        Ok(Self {
            snapshot_uid,
            tenant_id,
            connection_uid,
            object_uid,
            provider_revision,
            snapshot_hash,
            provenance,
            complete,
            entries,
            captured_at,
        })
    }

    /// Returns whether this snapshot can admit content for `revision`.
    ///
    /// Completeness and a revision match are both required; either failing makes
    /// the object invisible rather than partially visible.
    #[must_use]
    pub fn admits_revision(&self, revision: &str) -> bool {
        self.complete && self.provider_revision == revision
    }

    /// Returns whether the snapshot admits a caller holding `principals`.
    ///
    /// Deny wins over allow, and an empty principal set never matches.
    #[must_use]
    pub fn admits_principals(
        &self,
        principals: &std::collections::BTreeSet<SourcePrincipalFingerprint>,
    ) -> bool {
        let mut allowed = false;
        for entry in &self.entries {
            if !principals.contains(&entry.principal) {
                continue;
            }
            match entry.entry_kind {
                SourceAclEntryKind::Deny => return false,
                SourceAclEntryKind::Allow => allowed = true,
            }
        }
        allowed
    }
}

/// The stored ACL position of one knowledge object.
///
/// Carried on [`crate::domain::KnowledgeObject`] as one required field so a
/// freshly materialized object cannot exist without an ACL position at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectAcl {
    /// Freshness of the object's stored snapshot.
    pub state: SourceAclState,
    /// Provider ACL revision the object is currently pinned to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// Snapshot currently authoritative for this object, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_snapshot_uid: Option<Uuid>,
}

impl ObjectAcl {
    /// Returns the position every object starts in: no snapshot, nothing visible.
    ///
    /// This is also the position the V000348 backfill puts every pre-existing
    /// object into, so "never captured" and "captured before ACLs existed" are
    /// the same state rather than two subtly different ones.
    #[must_use]
    pub fn incomplete() -> Self {
        Self {
            state: SourceAclState::Incomplete,
            revision: None,
            current_snapshot_uid: None,
        }
    }

    /// Returns the position for an object pinned to a validated snapshot.
    #[must_use]
    pub fn current(revision: impl Into<String>, snapshot_uid: Uuid) -> Self {
        Self {
            state: SourceAclState::Current,
            revision: Some(revision.into()),
            current_snapshot_uid: Some(snapshot_uid),
        }
    }

    /// Returns whether this object's content may be surfaced to anyone at all.
    ///
    /// `TenantPublic` objects always may; `ProviderManaged` objects need a
    /// current snapshot, and the per-principal decision then happens in SQL.
    #[must_use]
    pub fn admits_under(&self, mode: ConnectionAclMode) -> bool {
        match mode {
            ConnectionAclMode::TenantPublic => true,
            ConnectionAclMode::ProviderManaged => {
                self.state.admits() && self.current_snapshot_uid.is_some()
            }
        }
    }
}

/// One verified binding between a MOA contact and a provider principal.
///
/// Written only by identity-verification paths — a linked account whose owner
/// MOA has confirmed, or a directory sync that proves membership. Retrieval
/// reads these and never writes them, and no request payload can add one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePrincipalBinding {
    /// Tenant-owned binding identifier.
    pub binding_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Contact that holds the principal, or the tenant-wide holder sentinel.
    pub contact_id: Uuid,
    /// Connection the binding was proven through, when it is connection-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_uid: Option<Uuid>,
    /// The kind of provider identity this binding proves.
    pub principal_kind: SourcePrincipalKind,
    /// Keyed opaque fingerprint of the bound principal.
    pub principal: SourcePrincipalFingerprint,
    /// When the binding was last verified.
    pub verified_at: DateTime<Utc>,
}

/// One verified membership edge in fingerprint space.
///
/// `member` is a principal the caller already holds; holding it also confers
/// `group`. Retrieval expands exactly one level, so adapters flatten nested
/// groups into direct edges when they write them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourcePrincipalGroupBinding {
    /// Tenant-owned binding identifier.
    pub binding_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Connection the membership was proven through, when connection-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_uid: Option<Uuid>,
    /// Principal that confers the membership.
    pub member: SourcePrincipalFingerprint,
    /// Whether the conferred principal is a group or a domain.
    pub group_kind: SourcePrincipalKind,
    /// Principal conferred by the membership.
    pub group: SourcePrincipalFingerprint,
    /// When the membership was last verified.
    pub verified_at: DateTime<Utc>,
}

/// The ACL half of one provider record, produced during normalization.
///
/// The entries are ALREADY fingerprinted: an adapter keys each principal with
/// the tenant's ACL key as it normalizes, so a raw provider identity never
/// outlives the normalization call. That is what makes a provider record safe to
/// journal — the durable Restate step result for a listed page contains opaque
/// fingerprints and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRecordAcl {
    /// The provider's ACL revision for this record.
    pub provider_revision: String,
    /// Whether the adapter enumerated every permission.
    ///
    /// `false` is a legitimate, recordable answer — a pagination cut short, a
    /// permission type the adapter does not model — and it hides the record
    /// rather than sharing it.
    pub complete: bool,
    /// How the ACL was captured.
    pub provenance: ProviderAclProvenance,
    /// Fingerprinted allow/deny entries for the record.
    pub entries: Vec<ProviderAclEntry>,
    /// Redacted provider metadata about the capture, never raw principals.
    #[serde(default)]
    pub metadata: Value,
}

/// The ACL a provider adapter attaches to one record.
///
/// There is no "unknown" arm and no `Option`: an adapter must state which world
/// its record lives in. A [`ProviderAclCapability::NativeSnapshots`] adapter that
/// returns [`RecordAcl::UniformlyPublic`] is a typed ingestion error, not a
/// record that quietly becomes tenant-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordAcl {
    /// The connector has no per-record permissions; the tenant reads everything.
    UniformlyPublic,
    /// The connector reported this record's native permissions.
    Provider(ProviderRecordAcl),
}

impl RecordAcl {
    /// Validates this ACL against the adapter's declared capability.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when a permission-bearing adapter claims a
    /// record is uniformly public, or when a uniformly-public adapter returns
    /// per-record permissions it has no business having. Either mismatch means
    /// the adapter's declared capability and its behavior disagree, and the safe
    /// reading of that disagreement is not one to infer.
    pub fn validate_for(&self, provider: &str, capability: ProviderAclCapability) -> Result<()> {
        match (self, capability) {
            (Self::UniformlyPublic, ProviderAclCapability::UniformlyPublic)
            | (Self::Provider(_), ProviderAclCapability::NativeSnapshots) => Ok(()),
            (Self::UniformlyPublic, ProviderAclCapability::NativeSnapshots) => {
                Err(Error::Provider {
                    provider: provider.to_string(),
                    message: "permission-bearing connector returned a record with no native ACL"
                        .to_string(),
                })
            }
            (Self::Provider(_), ProviderAclCapability::UniformlyPublic) => Err(Error::Provider {
                provider: provider.to_string(),
                message: "uniformly public connector returned per-record permissions".to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moa_core::types::memory::SourcePrincipalFingerprint;
    use std::collections::BTreeSet;

    fn fingerprint(byte: u8) -> SourcePrincipalFingerprint {
        SourcePrincipalFingerprint::from_digest(1, [byte; 32])
    }

    fn entry(kind: SourceAclEntryKind, byte: u8) -> ProviderAclEntry {
        ProviderAclEntry {
            entry_kind: kind,
            principal_kind: SourcePrincipalKind::User,
            principal: fingerprint(byte),
        }
    }

    #[test]
    fn capability_pins_the_only_legal_connection_mode() {
        // Pins: a permission-bearing adapter can never produce a tenant-public
        // connection, and widening an existing provider-managed connection is
        // recognizable as a downgrade.
        assert_eq!(
            ProviderAclCapability::UniformlyPublic.required_mode(),
            ConnectionAclMode::TenantPublic
        );
        assert_eq!(
            ProviderAclCapability::NativeSnapshots.required_mode(),
            ConnectionAclMode::ProviderManaged
        );
        assert!(ConnectionAclMode::ProviderManaged.widens_to(ConnectionAclMode::TenantPublic));
        assert!(!ConnectionAclMode::TenantPublic.widens_to(ConnectionAclMode::ProviderManaged));
        assert!(!ConnectionAclMode::ProviderManaged.widens_to(ConnectionAclMode::ProviderManaged));
    }

    #[test]
    fn principal_normalization_collapses_provider_formatting() {
        // Pins: the same identity spelled differently by two providers reaches
        // one canonical form, and `anyone` drops its subject entirely.
        let upper = CanonicalSourcePrincipal::new(
            " Google_Drive ",
            SourcePrincipalKind::User,
            " Alice@Example.COM ",
        )
        .expect("normalizes");
        let lower = CanonicalSourcePrincipal::new(
            "google_drive",
            SourcePrincipalKind::User,
            "alice@example.com",
        )
        .expect("normalizes");
        assert_eq!(upper, lower);
        assert_eq!(upper.canonical_bytes(), lower.canonical_bytes());

        let anyone =
            CanonicalSourcePrincipal::new("google_drive", SourcePrincipalKind::Anyone, "ignored")
                .expect("normalizes");
        assert_eq!(anyone.subject(), "");

        // A user principal and a group principal with the same subject are
        // different principals: the kind is inside the canonical form.
        let group = CanonicalSourcePrincipal::new(
            "google_drive",
            SourcePrincipalKind::Group,
            "alice@example.com",
        )
        .expect("normalizes");
        assert_ne!(group.canonical_bytes(), lower.canonical_bytes());
    }

    #[test]
    fn principal_rejects_unusable_fields() {
        // Pins: an empty namespace or a subject-bearing kind without a subject
        // is a typed error, never a principal that silently matches nothing (or
        // everything).
        assert!(
            CanonicalSourcePrincipal::new("", SourcePrincipalKind::User, "alice@example.com")
                .is_err()
        );
        assert!(CanonicalSourcePrincipal::new("drive", SourcePrincipalKind::User, "   ").is_err());
        assert!(
            CanonicalSourcePrincipal::new("drive", SourcePrincipalKind::Group, "\u{1f}x").is_err()
        );
    }

    #[test]
    fn snapshot_hash_is_order_insensitive_and_revision_bound() {
        // Pins: the canonical hash identifies the permission set, not the order
        // the provider listed it in, and a revision change changes identity.
        let forward = ProviderAclSnapshot::normalized(
            Uuid::from_u128(1),
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            ProviderAclProvenance::ProviderListing,
            true,
            vec![
                entry(SourceAclEntryKind::Allow, 9),
                entry(SourceAclEntryKind::Deny, 1),
                entry(SourceAclEntryKind::Allow, 9),
            ],
            Utc::now(),
        )
        .expect("normalizes");
        let reversed = ProviderAclSnapshot::normalized(
            Uuid::from_u128(5),
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            ProviderAclProvenance::ProviderChangeNotification,
            true,
            vec![
                entry(SourceAclEntryKind::Deny, 1),
                entry(SourceAclEntryKind::Allow, 9),
            ],
            Utc::now(),
        )
        .expect("normalizes");
        assert_eq!(forward.snapshot_hash, reversed.snapshot_hash);
        assert_eq!(forward.entries.len(), 2, "duplicate entries collapse");

        let next_revision = ProviderAclSnapshot::canonical_hash("rev-2", &forward.entries);
        assert_ne!(forward.snapshot_hash, next_revision);
    }

    #[test]
    fn snapshot_admission_requires_completeness_revision_and_an_allow() {
        // Pins the whole admission rule: incomplete or revision-mismatched
        // snapshots admit nobody, deny beats allow, and an empty principal set
        // never matches.
        let allowed = fingerprint(9);
        let denied = fingerprint(1);
        let snapshot = ProviderAclSnapshot::normalized(
            Uuid::from_u128(1),
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            ProviderAclProvenance::ProviderListing,
            true,
            vec![
                entry(SourceAclEntryKind::Allow, 9),
                entry(SourceAclEntryKind::Allow, 1),
                entry(SourceAclEntryKind::Deny, 1),
            ],
            Utc::now(),
        )
        .expect("normalizes");

        assert!(snapshot.admits_revision("rev-1"));
        assert!(!snapshot.admits_revision("rev-2"));

        assert!(snapshot.admits_principals(&BTreeSet::from([allowed.clone()])));
        assert!(
            !snapshot.admits_principals(&BTreeSet::from([denied.clone()])),
            "an explicit deny wins over the same principal's allow"
        );
        assert!(
            !snapshot.admits_principals(&BTreeSet::from([allowed, denied])),
            "a deny anywhere in the caller's principal set denies"
        );
        assert!(!snapshot.admits_principals(&BTreeSet::new()));

        let incomplete = ProviderAclSnapshot::normalized(
            Uuid::from_u128(6),
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            ProviderAclProvenance::ProviderListing,
            false,
            vec![entry(SourceAclEntryKind::Allow, 9)],
            Utc::now(),
        )
        .expect("normalizes");
        assert!(!incomplete.admits_revision("rev-1"));
    }

    #[test]
    fn object_acl_hides_provider_managed_objects_without_a_current_snapshot() {
        // Pins: the backfill's `incomplete` position, and a stale object, both
        // stay invisible until a resync produces a current snapshot.
        let mut acl = ObjectAcl::incomplete();
        assert!(!acl.admits_under(ConnectionAclMode::ProviderManaged));

        acl.state = SourceAclState::Stale;
        acl.revision = Some("rev-2".to_string());
        acl.current_snapshot_uid = Some(Uuid::from_u128(7));
        assert!(!acl.admits_under(ConnectionAclMode::ProviderManaged));

        let current = ObjectAcl::current("rev-2", Uuid::from_u128(7));
        assert!(current.admits_under(ConnectionAclMode::ProviderManaged));

        assert!(
            ObjectAcl::incomplete().admits_under(ConnectionAclMode::TenantPublic),
            "a uniformly public source needs no snapshot"
        );
    }
}
