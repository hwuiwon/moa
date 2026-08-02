//! Provider-native source ACL domain types.
//!
//! Every tenant knowledge connection carries the source system's own
//! permissions. A permission-bearing connector (Google Drive through Nango, a
//! Merge knowledge base) can hold documents that only some people in the tenant
//! may read, so MOA reproduces the provider's decision and never falls back to
//! "everyone in the tenant".
//!
//! The types in this module make that reproduction explicit and fail closed:
//!
//! * A [`ProviderAclSnapshot`] is immutable and carries the provider's own
//!   revision and a canonical hash. Admission requires a snapshot that is
//!   complete, current, and revision-matched.
//! * Principals are never stored in the clear. A [`CanonicalSourcePrincipal`] is
//!   normalized and then reduced to a keyed [`SourcePrincipalFingerprint`].

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::SourcePrincipalFingerprint;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

/// Field separator used when canonicalizing a principal before fingerprinting.
///
/// ASCII unit separator: it cannot appear in a provider namespace, kind, or
/// subject, so two different principals can never canonicalize to the same
/// string by splicing their parts together.
const PRINCIPAL_FIELD_SEPARATOR: char = '\u{1f}';

/// Maximum number of canonical entries persisted for one source ACL snapshot.
///
/// Provider snapshots above this bound are normalized to an incomplete snapshot
/// with no entries. That preserves fail-closed visibility without letting one
/// provider record drive an unbounded database write.
pub const MAX_SOURCE_ACL_ENTRIES: usize = 4096;

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

    /// Returns whether this state can admit the governed object at all.
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
    /// Computed over completeness, the sorted deduplicated entries, and the
    /// provider revision. A partial listing must not collide with a later
    /// complete listing of the entries seen so far.
    #[must_use]
    pub fn canonical_hash(
        provider_revision: &str,
        complete: bool,
        entries: &[ProviderAclEntry],
    ) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"moa/source-acl/snapshot/v1");
        hasher.update(&(provider_revision.len() as u32).to_be_bytes());
        hasher.update(provider_revision.as_bytes());
        hasher.update(&[u8::from(complete)]);
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
    /// A canonical entry set above [`MAX_SOURCE_ACL_ENTRIES`] is represented as
    /// incomplete with zero entries, so it can be persisted to hide the object
    /// without attempting an unbounded write.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Provider`] when the provider revision is empty: without
    /// a revision there is nothing to compare a later provider announcement
    /// against, so staleness could never be detected.
    pub fn normalized(
        tenant_id: TenantId,
        connection_uid: Uuid,
        object_uid: Uuid,
        provider_revision: impl Into<String>,
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
        let mut canonical = BTreeSet::new();
        let mut oversized = false;
        for entry in entries {
            canonical.insert(entry);
            if canonical.len() > MAX_SOURCE_ACL_ENTRIES {
                oversized = true;
                break;
            }
        }
        let complete = complete && !oversized;
        let entries = if oversized {
            Vec::new()
        } else {
            canonical.into_iter().collect()
        };
        let snapshot_hash = Self::canonical_hash(&provider_revision, complete, &entries);
        let snapshot_uid = crate::graph_delta::stable_uid(&format!(
            "source-acl-snapshot:{object_uid}:{snapshot_hash}"
        ));
        Ok(Self {
            snapshot_uid,
            tenant_id,
            connection_uid,
            object_uid,
            provider_revision,
            snapshot_hash,
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
    /// A new object remains in this fail-closed state until ingestion captures
    /// a complete provider ACL snapshot.
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

    /// Returns whether this object's content has a current source ACL snapshot.
    ///
    /// The per-principal decision happens in SQL after this structural check.
    #[must_use]
    pub fn admits(&self) -> bool {
        self.state.admits() && self.current_snapshot_uid.is_some()
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
/// the tenant's ACL key while converting the provider response and strips the
/// readable permission fields before returning the record. That makes the
/// durable Restate page result safe to journal.
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
    /// Fingerprinted allow/deny entries for the record.
    pub entries: Vec<ProviderAclEntry>,
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

    fn indexed_entry(index: usize) -> ProviderAclEntry {
        let mut digest = [0_u8; 32];
        digest[..8].copy_from_slice(&(index as u64).to_be_bytes());
        ProviderAclEntry {
            entry_kind: SourceAclEntryKind::Allow,
            principal_kind: SourcePrincipalKind::User,
            principal: SourcePrincipalFingerprint::from_digest(1, digest),
        }
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
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
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
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            true,
            vec![
                entry(SourceAclEntryKind::Deny, 1),
                entry(SourceAclEntryKind::Allow, 9),
            ],
            Utc::now(),
        )
        .expect("normalizes");
        assert_eq!(forward.snapshot_hash, reversed.snapshot_hash);
        assert_eq!(forward.snapshot_uid, reversed.snapshot_uid);
        assert_eq!(forward.entries.len(), 2, "duplicate entries collapse");

        let next_revision = ProviderAclSnapshot::canonical_hash("rev-2", true, &forward.entries);
        assert_ne!(forward.snapshot_hash, next_revision);
        assert_ne!(
            forward.snapshot_hash,
            ProviderAclSnapshot::canonical_hash("rev-1", false, &forward.entries),
            "a partial listing must not occupy the complete snapshot's identity"
        );

        let changed_entries = ProviderAclSnapshot::normalized(
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            true,
            vec![entry(SourceAclEntryKind::Allow, 8)],
            Utc::now(),
        )
        .expect("normalizes");
        assert_ne!(forward.snapshot_uid, changed_entries.snapshot_uid);
    }

    #[test]
    fn snapshot_normalization_caps_the_canonical_entry_set_fail_closed() {
        // Pins: the exact canonical limit remains complete, while one additional
        // unique entry produces persistable incomplete evidence with no partial
        // permission entries. Raw duplicates do not consume the canonical cap.
        let tenant_id = TenantId::from(Uuid::from_u128(2));
        let connection_uid = Uuid::from_u128(3);
        let object_uid = Uuid::from_u128(4);
        let captured_at = Utc::now();
        let at_limit = ProviderAclSnapshot::normalized(
            tenant_id,
            connection_uid,
            object_uid,
            "rev-limit",
            true,
            (0..MAX_SOURCE_ACL_ENTRIES).map(indexed_entry).collect(),
            captured_at,
        )
        .expect("the exact canonical limit normalizes");
        assert!(at_limit.complete);
        assert_eq!(at_limit.entries.len(), MAX_SOURCE_ACL_ENTRIES);

        let oversized = ProviderAclSnapshot::normalized(
            tenant_id,
            connection_uid,
            object_uid,
            "rev-oversized",
            true,
            (0..=MAX_SOURCE_ACL_ENTRIES).map(indexed_entry).collect(),
            captured_at,
        )
        .expect("oversized ACLs normalize to fail-closed evidence");
        assert!(!oversized.complete);
        assert_eq!(oversized.entries.len(), 0);
        assert_eq!(
            oversized.snapshot_hash,
            ProviderAclSnapshot::canonical_hash("rev-oversized", false, &[])
        );

        let duplicate_heavy = ProviderAclSnapshot::normalized(
            tenant_id,
            connection_uid,
            object_uid,
            "rev-duplicates",
            true,
            vec![indexed_entry(7); MAX_SOURCE_ACL_ENTRIES + 1],
            captured_at,
        )
        .expect("raw duplicates normalize before the cap is applied");
        assert!(duplicate_heavy.complete);
        assert_eq!(duplicate_heavy.entries, vec![indexed_entry(7)]);
    }

    #[test]
    fn snapshot_admission_requires_completeness_revision_and_an_allow() {
        // Pins the whole admission rule: incomplete or revision-mismatched
        // snapshots admit nobody, deny beats allow, and an empty principal set
        // never matches.
        let allowed = fingerprint(9);
        let denied = fingerprint(1);
        let snapshot = ProviderAclSnapshot::normalized(
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
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
            TenantId::from(Uuid::from_u128(2)),
            Uuid::from_u128(3),
            Uuid::from_u128(4),
            "rev-1",
            false,
            vec![entry(SourceAclEntryKind::Allow, 9)],
            Utc::now(),
        )
        .expect("normalizes");
        assert!(!incomplete.admits_revision("rev-1"));
    }

    #[test]
    fn object_acl_hides_objects_without_a_current_snapshot() {
        // Pins: the backfill's `incomplete` position, and a stale object, both
        // stay invisible until a resync produces a current snapshot.
        let mut acl = ObjectAcl::incomplete();
        assert!(!acl.admits());

        acl.state = SourceAclState::Stale;
        acl.revision = Some("rev-2".to_string());
        acl.current_snapshot_uid = Some(Uuid::from_u128(7));
        assert!(!acl.admits());

        let current = ObjectAcl::current("rev-2", Uuid::from_u128(7));
        assert!(current.admits());
    }
}
