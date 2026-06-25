//! Privacy export and erasure wire DTOs.

use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

/// Response payload for a privacy erase request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyEraseResponse {
    /// Tenant containing the erased subject data.
    pub tenant_id: TenantId,
    /// Subject user identifier erased.
    pub subject_user_id: UserId,
    /// Number of candidate memory nodes found.
    pub candidate_count: u64,
    /// Number of memory nodes erased.
    pub erased_count: u64,
    /// Number of PII vault rows erased.
    pub pii_vault_erased: u64,
    /// Whether the request was a dry run.
    pub dry_run: bool,
    /// Sample erase candidates for dry-run output.
    #[serde(default)]
    pub sample: Vec<Value>,
}
