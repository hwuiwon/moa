//! Privacy operation context and subject-scope helpers.

use moa_config::ComplianceConfig;
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::UserId,
};
use moa_wire::privacy::{
    ContactErasureScope, ParsedPrivacySubjectId, PrivacyEraseRequest,
    contact_privacy_subject_string,
};
use restate_sdk::prelude::*;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

use super::approval::ApprovalClaims;
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
pub struct PrivacyEraseContext {
    /// Postgres pool used for graph and PII-vault erasure.
    pub pool: PgPool,
    /// Required key-management provider used for fail-closed crypto-shred.
    pub kms: Arc<dyn moa_crypto::KeyManagementProvider>,
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
    /// When true, a distinct second-admin dual-control approval must be consumed
    /// before this erasure may execute (four-eyes / segregation of duties). Off by
    /// default, preserving single-admin erasure.
    pub require_dual_control: bool,
}

impl PrivacyEraseContext {
    /// Builds an erase context from the public wire request and verified claims.
    pub fn from_request(
        pool: PgPool,
        request: PrivacyEraseRequest,
        claims: ApprovalClaims,
        config: &ComplianceConfig,
        kms: Arc<dyn moa_crypto::KeyManagementProvider>,
    ) -> Result<Self, HandlerError> {
        let subject_user = parse_subject_uuid(&request.subject_user_id)?;
        let storage_partition_id = storage_partition_id_for_tenant(request.tenant_id);
        Ok(Self {
            pool,
            kms,
            tenant_id: request.tenant_id,
            storage_partition_id: storage_partition_id.to_string(),
            subject_user,
            subject_user_id: request.subject_user_id.to_string(),
            reason: request.reason,
            dry_run: request.dry_run,
            contact_erasure_scope: request.contact_erasure_scope,
            claims,
            pii_vault_secret: pii_vault_secret_from_config(config)?,
            require_dual_control: config.require_dual_control_for_erasure,
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
            user_id: contact_privacy_subject_string(ContactId(contact_id)),
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

fn pii_vault_secret_from_config(
    config: &ComplianceConfig,
) -> Result<Option<Vec<u8>>, HandlerError> {
    config
        .pii_vault_secret_hex
        .as_deref()
        .map(|secret_hex| {
            hex::decode(secret_hex.trim()).map_err(|error| {
                TerminalError::new_with_code(
                    400,
                    format!("MOA_PII_VAULT_SECRET_HEX must be hex-encoded: {error}"),
                )
                .into()
            })
        })
        .transpose()
}

fn parse_subject_uuid(subject_user_id: &UserId) -> Result<Uuid, HandlerError> {
    ParsedPrivacySubjectId::parse(subject_user_id)
        .map(|parsed| parsed.uuid)
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}
