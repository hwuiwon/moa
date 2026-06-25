//! Privacy operation context and subject-scope helpers.

use moa_core::wire::privacy::{ContactErasureScope, PrivacyEraseRequest};
use moa_core::{StoragePartitionId, TenantId, UserId};
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

use super::approval::ApprovalClaims;
use super::repository::parse_privacy_subject_id;

const PII_VAULT_SECRET_HEX_ENV: &str = "MOA_PII_VAULT_SECRET_HEX";
/// Prefix that identifies privacy subjects backed by contacts.
pub(super) const CONTACT_SUBJECT_PREFIX: &str = "contact:";

/// Context for one server-side privacy export.
#[derive(Debug)]
pub struct PrivacyExportContext {
    /// Postgres pool used for privacy reads and audit writes.
    pub pool: PgPool,
    /// Tenant that authorized the privacy operation.
    pub tenant_id: TenantId,
    /// Storage partition derived from the tenant id.
    pub storage_partition: Option<String>,
    /// Subject user UUID.
    pub subject_user: Uuid,
    /// Subject user id as stored in text columns.
    pub subject_user_id: String,
    /// Effective subject ids included in export collection.
    pub subjects: Vec<PrivacySubject>,
    /// Administrative reason for the export.
    pub reason: String,
    /// Verified approval-token claims.
    pub claims: ApprovalClaims,
}

/// Context for one server-side privacy erase.
#[derive(Debug)]
pub struct PrivacyEraseContext {
    /// Postgres pool used for graph and PII-vault erasure.
    pub pool: PgPool,
    /// Tenant that authorized the privacy operation.
    pub tenant_id: TenantId,
    /// Storage partition derived from the tenant id.
    pub storage_partition_id: String,
    /// Subject user UUID.
    pub subject_user: Uuid,
    /// Subject user id as stored in text columns.
    pub subject_user_id: String,
    /// Administrative reason for the erasure.
    pub reason: String,
    /// Whether to enumerate candidates without writing erasures.
    pub dry_run: bool,
    /// Explicit contact erasure boundary, required for contact subjects.
    pub contact_erasure_scope: Option<ContactErasureScope>,
    /// Verified approval-token claims.
    pub claims: ApprovalClaims,
    /// Optional PII vault secret used to compute subject pseudonyms.
    pub pii_vault_secret: Option<Vec<u8>>,
}

impl PrivacyEraseContext {
    /// Builds an erase context from the public wire request and verified claims.
    pub fn from_request(
        pool: PgPool,
        request: PrivacyEraseRequest,
        claims: ApprovalClaims,
    ) -> Result<Self, HandlerError> {
        let subject_user = parse_subject_uuid(&request.subject_user_id)?;
        let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
        Ok(Self {
            pool,
            tenant_id: request.tenant_id,
            storage_partition_id: storage_partition_id.to_string(),
            subject_user,
            subject_user_id: request.subject_user_id.to_string(),
            reason: request.reason,
            dry_run: request.dry_run,
            contact_erasure_scope: request.contact_erasure_scope,
            claims,
            pii_vault_secret: pii_vault_secret_from_env()?,
        })
    }
}

/// Privacy export or erasure subject included in a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacySubject {
    /// User id string as stored in memory tables.
    pub user_id: String,
    /// Stable UUID target used by privacy audit rows.
    pub target_uid: Uuid,
    /// Why this subject is included in the request.
    pub provenance: PrivacySubjectProvenance,
}

impl PrivacySubject {
    /// Builds the primary subject requested by the caller.
    #[must_use]
    pub fn primary(user_id: String, target_uid: Uuid) -> Self {
        Self {
            user_id,
            target_uid,
            provenance: PrivacySubjectProvenance::Primary,
        }
    }

    /// Builds a linked-contact subject included through contact promotion.
    pub(super) fn linked_contact(contact_id: Uuid) -> Self {
        Self {
            user_id: format!("{CONTACT_SUBJECT_PREFIX}{contact_id}"),
            target_uid: contact_id,
            provenance: PrivacySubjectProvenance::LinkedContact,
        }
    }
}

/// Subject provenance included in privacy artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivacySubjectProvenance {
    /// Subject was the one explicitly requested.
    Primary,
    /// Subject is a linked contact included through verified contact promotion.
    LinkedContact,
}

impl PrivacySubjectProvenance {
    /// Returns the stable manifest/audit string for this provenance.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::LinkedContact => "linked_contact",
        }
    }
}

/// Returns the storage partition used by tenant-scoped privacy operations.
pub(super) fn storage_partition_id_for_tenant(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

fn pii_vault_secret_from_env() -> Result<Option<Vec<u8>>, HandlerError> {
    std::env::var(PII_VAULT_SECRET_HEX_ENV)
        .ok()
        .map(|secret_hex| {
            hex::decode(secret_hex.trim()).map_err(|error| {
                TerminalError::new_with_code(
                    400,
                    format!("{PII_VAULT_SECRET_HEX_ENV} must be hex-encoded: {error}"),
                )
                .into()
            })
        })
        .transpose()
}

fn parse_subject_uuid(subject_user_id: &UserId) -> Result<Uuid, HandlerError> {
    parse_privacy_subject_id(subject_user_id).map(|parsed| parsed.uuid)
}
