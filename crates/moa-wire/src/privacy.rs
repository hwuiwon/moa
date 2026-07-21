//! Privacy export and erasure wire DTOs.

use moa_core::{
    types::contact::ContactId,
    types::identifiers::{TenantId, UserId},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Prefix used when a privacy subject id identifies an agent-facing contact.
pub const CONTACT_PRIVACY_SUBJECT_PREFIX: &str = "contact:";

/// Effective kind encoded by a privacy subject id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacySubjectIdKind {
    /// The subject id is an ordinary UUID-backed user id.
    User,
    /// The subject id is a contact id encoded with the contact prefix.
    Contact,
}

/// Parsed privacy subject id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedPrivacySubjectId {
    /// Parsed UUID value.
    pub uuid: Uuid,
    /// Subject kind encoded by the original id.
    pub kind: PrivacySubjectIdKind,
}

impl ParsedPrivacySubjectId {
    /// Parses a privacy subject id.
    pub fn parse(
        subject_user_id: &UserId,
    ) -> std::result::Result<Self, PrivacySubjectIdParseError> {
        Self::parse_str(subject_user_id.as_str())
    }

    /// Parses a privacy subject id from a string slice.
    pub fn parse_str(raw: &str) -> std::result::Result<Self, PrivacySubjectIdParseError> {
        let (value, kind) = raw
            .strip_prefix(CONTACT_PRIVACY_SUBJECT_PREFIX)
            .map_or((raw, PrivacySubjectIdKind::User), |value| {
                (value, PrivacySubjectIdKind::Contact)
            });
        let uuid = Uuid::parse_str(value)?;
        Ok(Self { uuid, kind })
    }

    /// Returns true when the original subject id used the contact prefix.
    #[must_use]
    pub fn is_contact(self) -> bool {
        self.kind == PrivacySubjectIdKind::Contact
    }

    /// Interprets the parsed UUID as a contact id.
    #[must_use]
    pub fn contact_id(self) -> ContactId {
        ContactId(self.uuid)
    }
}

/// Formats a contact id as a privacy subject id string.
#[must_use]
pub fn contact_privacy_subject_string(contact_id: ContactId) -> String {
    format!("{CONTACT_PRIVACY_SUBJECT_PREFIX}{}", contact_id.0)
}

/// Error returned when a privacy subject id cannot be parsed.
#[derive(Debug, thiserror::Error)]
#[error("subject_user_id must be a UUID-backed user id: {0}")]
pub struct PrivacySubjectIdParseError(#[from] uuid::Error);

/// Request payload for exporting privacy data for one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyExportRequest {
    /// Tenant containing the subject data to export.
    pub tenant_id: TenantId,
    /// Subject user identifier for the data export.
    pub subject_user_id: UserId,
    /// Administrative reason recorded in the audit trail.
    pub reason: String,
    /// Signed platform-admin approval token.
    pub approval_token: String,
    /// Optional armored PGP recipient key for encrypting the archive.
    pub pgp_recipient: Option<String>,
}

/// Response payload describing a privacy export archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyExportResponse {
    /// Subject user identifier exported.
    pub subject_user_id: UserId,
    /// Tenant containing the exported subject data.
    pub tenant_id: TenantId,
    /// URI where the archive can be fetched.
    pub archive_uri: String,
    /// Number of files included in the archive.
    pub file_count: u64,
    /// Per-section exported row counts.
    #[serde(default)]
    pub counts: BTreeMap<String, u64>,
    /// Optional manifest or signature details.
    #[serde(default)]
    pub manifest: Value,
    /// Optional base64-encoded archive bytes for API file output.
    pub archive_base64: Option<String>,
}

/// Request payload for erasing privacy data for one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyEraseRequest {
    /// Tenant containing the subject data to erase.
    pub tenant_id: TenantId,
    /// Subject user identifier for the erasure request.
    pub subject_user_id: UserId,
    /// Administrative reason recorded in the audit trail.
    pub reason: String,
    /// Whether to list candidates without writing graph or changelog rows.
    #[serde(default)]
    pub dry_run: bool,
    /// Explicit contact erasure boundary when the subject is a contact.
    #[serde(default)]
    pub contact_erasure_scope: Option<ContactErasureScope>,
    /// Signed platform-admin approval token.
    pub approval_token: String,
}

/// Erasure boundary for contact privacy requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactErasureScope {
    /// Erase only the requested contact subject.
    SpecifiedContact,
    /// Erase the requested contact and linked unverified contacts.
    SpecifiedAndLinkedContacts,
}

/// Terminal status of a privacy erase request.
///
/// The erase handler is synchronous within its Restate operation, so a
/// successful call returns a terminal status. Non-terminal `running`/`failed`
/// states are persisted on the durable `moa.erasure_jobs` row for resume and
/// audit; a caller only observes them through job introspection, never as a
/// successful response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyEraseStatus {
    /// Candidates were enumerated without writing any erasure.
    DryRun,
    /// Every attributable store reached its erased end state.
    Completed,
    /// Erasure was refused because the subject (or its tenant) is under an active
    /// legal hold that overrides right-to-erasure until the hold is released. No
    /// data was purged and no crypto-shred ran.
    BlockedByLegalHold,
}

/// Response payload for a privacy erase request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyEraseResponse {
    /// Tenant containing the erased subject data.
    pub tenant_id: TenantId,
    /// Subject user identifier erased.
    pub subject_user_id: UserId,
    /// Terminal status of the erasure operation.
    pub status: PrivacyEraseStatus,
    /// Number of candidate memory nodes found.
    pub candidate_count: u64,
    /// Number of memory nodes erased.
    pub erased_count: u64,
    /// Number of PII vault rows erased.
    pub pii_vault_erased: u64,
    /// Number of standing memory-digest rows deleted.
    #[serde(default)]
    pub digest_deleted: u64,
    /// Number of retrieval-lineage rows deleted.
    #[serde(default)]
    pub lineage_deleted: u64,
    /// Whether the request was a dry run.
    pub dry_run: bool,
    /// Sample erase candidates for dry-run output.
    #[serde(default)]
    pub sample: Vec<Value>,
}

/// Request from the first tenant admin to raise a four-eyes dual-control approval
/// for a privacy erasure, before that erasure may execute.
///
/// The operation parameters (tenant, subject, scope, reason) must match the
/// eventual erase request exactly; the server derives a canonical fingerprint from
/// them so the resulting approval binds to one specific erasure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestErasureApprovalRequest {
    /// Tenant whose subject data the erasure would remove.
    pub tenant_id: TenantId,
    /// Subject user identifier the erasure would target.
    pub subject_user_id: UserId,
    /// Administrative reason recorded with the erasure request.
    pub reason: String,
    /// Explicit contact erasure boundary when the subject is a contact.
    #[serde(default)]
    pub contact_erasure_scope: Option<ContactErasureScope>,
}

/// Response identifying the pending dual-control request a second admin approves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestErasureApprovalResponse {
    /// Identifier of the pending dual-control request.
    pub request_id: Uuid,
}

/// Approval from a second, distinct tenant admin for a pending dual-control
/// request. The approver must differ from the requester (segregation of duties).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApproveDualControlRequest {
    /// Tenant that owns the pending request.
    pub tenant_id: TenantId,
    /// Identifier of the pending dual-control request to approve.
    pub request_id: Uuid,
}

/// Response describing a dual-control approval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApproveDualControlResponse {
    /// True when the request moved from pending to approved.
    pub approved: bool,
}

/// Request to place a legal hold on a subject, or on the whole tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceLegalHoldRequest {
    /// Tenant whose data the hold preserves.
    pub tenant_id: TenantId,
    /// Subject under hold; `None` places a tenant-wide hold covering every subject.
    #[serde(default)]
    pub subject_id: Option<Uuid>,
    /// Administrative reason recorded with the hold.
    pub reason: String,
}

/// Response describing a placed legal hold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceLegalHoldResponse {
    /// Identifier of the placed hold, used to release it later.
    pub hold_id: Uuid,
    /// Tenant the hold belongs to.
    pub tenant_id: TenantId,
    /// Subject under hold; `None` for a tenant-wide hold.
    pub subject_id: Option<Uuid>,
}

/// Request to release an active legal hold by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseLegalHoldRequest {
    /// Tenant that owns the hold.
    pub tenant_id: TenantId,
    /// Identifier of the hold to release.
    pub hold_id: Uuid,
}

/// Response describing a legal-hold release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseLegalHoldResponse {
    /// Identifier of the hold targeted for release.
    pub hold_id: Uuid,
    /// True when an active hold was released; false when it was already released
    /// or does not exist in the tenant.
    pub released: bool,
}
