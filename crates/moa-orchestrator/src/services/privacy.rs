//! Restate service for protected privacy export and erasure operations.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as BASE64_STANDARD, URL_SAFE_NO_PAD};
use chrono::{TimeZone, Utc};
use ed25519_dalek::{Signature, Signer as DalekSigner, SigningKey, Verifier, VerifyingKey};
use flate2::Compression;
use flate2::write::GzEncoder;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::{
    ContactErasureScope, PrivacyEraseRequest, PrivacyEraseResponse, PrivacyExportRequest,
    PrivacyExportResponse,
};
use moa_core::{TenantId, UserId, WorkspaceId};
use moa_lineage_audit::PiiVault;
use moa_memory_graph::{ChangelogRecord, write_and_bump};
use moa_memory_pii::erasure::{
    EraseCandidate, GraphErasureAudit, enumerate_erase_candidates, hard_purge_erase_candidates,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tar::Builder;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

const APPROVAL_PUBLIC_KEY_ENV: &str = "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX";
const EXPORT_SIGNING_KEY_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX";
const EXPORT_SIGNING_KEY_ID_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_ID";
const PII_VAULT_SECRET_HEX_ENV: &str = "MOA_PII_VAULT_WORKSPACE_SECRET_HEX";
const ERASE_SAMPLE_LIMIT: usize = 20;
const CONTACT_SUBJECT_PREFIX: &str = "contact:";

/// Restate service surface for protected privacy administration.
#[restate_sdk::service]
#[name = "Privacy"]
pub trait Privacy {
    /// Exports privacy data for one subject after admin authorization.
    async fn export(
        request: Json<PrivacyExportRequest>,
    ) -> Result<Json<PrivacyExportResponse>, HandlerError>;

    /// Erases privacy data for one subject after admin authorization.
    async fn erase(
        request: Json<PrivacyEraseRequest>,
    ) -> Result<Json<PrivacyEraseResponse>, HandlerError>;
}

/// Concrete privacy service implementation.
#[derive(Clone, Default)]
pub struct PrivacyImpl;

impl Privacy for PrivacyImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<PrivacyExportRequest>,
    ) -> Result<Json<PrivacyExportResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "export");
        let request = request.into_inner();
        authorize_tenant_or_workspace_admin(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let storage_workspace_id = storage_workspace_id_for_tenant(request.tenant_id);
        let workspace = Some(storage_workspace_id.as_str());
        let claims = ApprovalTokenVerifier::from_env()?.verify(
            &request.approval_token,
            "export",
            &subject_user_id,
            workspace,
        )?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                execute_privacy_export(pool, request.tenant_id, request, claims)
                    .await
                    .map(Json::from)
            })
            .name("privacy_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn erase(
        &self,
        ctx: Context<'_>,
        request: Json<PrivacyEraseRequest>,
    ) -> Result<Json<PrivacyEraseResponse>, HandlerError> {
        annotate_restate_handler_span("Privacy", "erase");
        let request = request.into_inner();
        authorize_tenant_or_workspace_admin(&ctx, request.tenant_id, Relation::Admin).await?;
        let subject_user_id = request.subject_user_id.to_string();
        let storage_workspace_id = storage_workspace_id_for_tenant(request.tenant_id);
        let workspace = Some(storage_workspace_id.as_str());
        let claims = ApprovalTokenVerifier::from_env()?.verify(
            &request.approval_token,
            "erase",
            &subject_user_id,
            workspace,
        )?;
        let pool = OrchestratorCtx::current_graph_pool();
        let erase_ctx = PrivacyEraseContext::from_request(pool, request, claims)?;

        Ok(ctx
            .run(|| async move { run_privacy_erase(erase_ctx).await.map(Json::from) })
            .name("privacy_erase")
            .await?)
    }
}

/// Signed approval-token claims required before privacy operations touch data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalClaims {
    /// Approving administrator subject identifier.
    pub sub: String,
    /// Unique token identifier used for replay protection.
    pub jti: String,
    /// Expiration timestamp as Unix seconds.
    pub exp: i64,
    /// Approved operation, such as `export` or `erase`.
    pub op: String,
    /// Subject user identifier covered by the approval.
    pub subject_user_id: String,
    /// Optional workspace identifier covered by the approval.
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// Optional single role claim.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional role list claim.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl ApprovalClaims {
    fn has_platform_admin_role(&self) -> bool {
        self.role.as_deref() == Some("platform_admin")
            || self.roles.iter().any(|role| role == "platform_admin")
    }
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: String,
}

/// Verifies compact EdDSA approval JWTs used for privacy operations.
pub struct ApprovalTokenVerifier {
    /// Ed25519 public key that verifies approval tokens.
    pub verifying_key: VerifyingKey,
}

impl ApprovalTokenVerifier {
    /// Builds a verifier from the configured approval public key environment.
    pub fn from_env() -> Result<Self, HandlerError> {
        let raw = std::env::var(APPROVAL_PUBLIC_KEY_ENV)
            .map_err(|_| TerminalError::new(format!("{APPROVAL_PUBLIC_KEY_ENV} is required")))?;
        Self::from_public_key_material(&raw)
    }

    /// Builds a verifier from hex or base64 public key material.
    pub fn from_public_key_material(raw: &str) -> Result<Self, HandlerError> {
        let bytes = decode_key_material(raw)?;
        let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "approval public key must be 32 bytes")
        })?;
        Ok(Self {
            verifying_key: VerifyingKey::from_bytes(&key_bytes).map_err(handler_error)?,
        })
    }

    /// Verifies an approval JWT for one operation, subject, and optional workspace.
    pub fn verify(
        &self,
        token: &str,
        expected_op: &str,
        subject_user_id: &str,
        workspace: Option<&str>,
    ) -> Result<ApprovalClaims, HandlerError> {
        let parts = token.split('.').collect::<Vec<_>>();
        if parts.len() != 3 {
            return Err(
                TerminalError::new_with_code(400, "approval token must be a compact JWT").into(),
            );
        }

        let header: JwtHeader =
            serde_json::from_slice(&decode_base64url(parts[0])?).map_err(handler_error)?;
        if header.alg != "EdDSA" {
            return Err(TerminalError::new_with_code(400, "approval token must use EdDSA").into());
        }

        let claims: ApprovalClaims =
            serde_json::from_slice(&decode_base64url(parts[1])?).map_err(handler_error)?;
        validate_claims(&claims, expected_op, subject_user_id, workspace)?;

        let signature_bytes = decode_base64url(parts[2])?;
        let signature_bytes: [u8; 64] = signature_bytes.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "approval token signature must be 64 bytes")
        })?;
        let signature = Signature::from_bytes(&signature_bytes);
        let signed = format!("{}.{}", parts[0], parts[1]);
        self.verifying_key
            .verify(signed.as_bytes(), &signature)
            .map_err(handler_error)?;
        Ok(claims)
    }
}

/// Ed25519 signer for generated privacy export manifests.
pub struct Ed25519ManifestSigner {
    /// Stable key identifier recorded in manifests.
    pub key_id: String,
    /// Ed25519 signing key.
    pub signing_key: SigningKey,
}

impl Ed25519ManifestSigner {
    /// Builds a manifest signer from configured signing key environment.
    pub fn from_env() -> Result<Self, HandlerError> {
        let raw = std::env::var(EXPORT_SIGNING_KEY_ENV)
            .map_err(|_| TerminalError::new(format!("{EXPORT_SIGNING_KEY_ENV} is required")))?;
        let key_id = std::env::var(EXPORT_SIGNING_KEY_ID_ENV)
            .unwrap_or_else(|_| "moa-privacy-export-ops".to_string());
        Self::from_signing_key_material(key_id, &raw)
    }

    /// Builds a manifest signer from hex or base64 private key material.
    pub fn from_signing_key_material(key_id: String, raw: &str) -> Result<Self, HandlerError> {
        let bytes = decode_key_material(raw)?;
        let seed = match bytes.len() {
            32 => bytes,
            64 => bytes[..32].to_vec(),
            len => {
                return Err(TerminalError::new_with_code(
                    400,
                    format!("export signing key must be 32 or 64 bytes, got {len}"),
                )
                .into());
            }
        };
        let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "export signing key must be 32 bytes")
        })?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    /// Returns the manifest key identifier.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Returns the Ed25519 public key as hex.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    /// Signs exact manifest bytes.
    pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        self.signing_key.sign(bytes).to_bytes().to_vec()
    }
}

/// Context for one server-side privacy export.
#[derive(Debug)]
pub struct PrivacyExportContext {
    /// Postgres pool used for privacy reads and audit writes.
    pub pool: PgPool,
    /// Tenant that authorized the privacy operation.
    pub tenant_id: TenantId,
    /// Storage workspace partition derived from the tenant id.
    pub workspace: Option<String>,
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
    /// Storage workspace partition derived from the tenant id.
    pub workspace_id: String,
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
        let storage_workspace_id = storage_workspace_id_for_tenant(request.tenant_id);
        Ok(Self {
            pool,
            tenant_id: request.tenant_id,
            workspace_id: storage_workspace_id.to_string(),
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

    fn linked_contact(contact_id: Uuid) -> Self {
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
    fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::LinkedContact => "linked_contact",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrivacySubjectKind {
    User,
    Contact,
}

#[derive(Debug, Clone)]
struct ResolvedPrivacySubjects {
    kind: PrivacySubjectKind,
    effective_workspace: Option<String>,
    subjects: Vec<PrivacySubject>,
}

async fn authorize_tenant_or_workspace_admin(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    if require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .is_ok()
    {
        return Ok(identity);
    }
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        &global_workspace_id(),
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

fn global_workspace_id() -> WorkspaceId {
    WorkspaceId::new("workspace")
}

fn storage_workspace_id_for_tenant(tenant_id: TenantId) -> WorkspaceId {
    WorkspaceId::new(tenant_id.to_string())
}

async fn execute_privacy_export(
    pool: PgPool,
    tenant_id: TenantId,
    request: PrivacyExportRequest,
    claims: ApprovalClaims,
) -> Result<PrivacyExportResponse, HandlerError> {
    if request.reason.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "--reason is required").into());
    }
    consume_approval_jti(&pool, &claims).await?;
    let storage_workspace_id = storage_workspace_id_for_tenant(tenant_id);
    let resolved = resolve_privacy_subjects(
        &pool,
        tenant_id.0,
        Some(&storage_workspace_id),
        &request.subject_user_id,
        ContactLinkedSubjectPolicy::IncludeVerifiedLinks,
    )
    .await?;
    let subject_user = resolved
        .subjects
        .first()
        .map(|subject| subject.target_uid)
        .ok_or_else(|| TerminalError::new("privacy subject resolution returned no subjects"))?;
    let workspace = resolved.effective_workspace.clone();
    let signer = Ed25519ManifestSigner::from_env()?;
    let base_dir = create_temp_dir("moa-privacy-export").await?;
    let export_dir = base_dir.join("export");
    tokio::fs::create_dir_all(&export_dir)
        .await
        .map_err(handler_error)?;
    let ctx = PrivacyExportContext {
        pool,
        tenant_id,
        workspace,
        subject_user,
        subject_user_id: request.subject_user_id.to_string(),
        subjects: resolved.subjects,
        reason: request.reason,
        claims,
    };

    let result = async {
        let mut counts = collect_privacy_export_data_sections(&ctx, &export_dir).await?;
        write_export_readme(&ctx, &counts, &export_dir).await?;
        emit_export_audit(&ctx, &counts).await?;
        counts.insert("changelog", collect_changelog(&ctx, &export_dir).await?);
        let manifest = write_manifest(&export_dir, &signer, &ctx, &counts).await?;
        let archive =
            finalize_archive_to_bytes(&export_dir, request.pgp_recipient.as_deref()).await?;
        Ok::<_, HandlerError>((counts, manifest, archive))
    }
    .await;

    cleanup_temp_dir(&base_dir).await;
    let (counts, manifest, archive) = result?;
    Ok(PrivacyExportResponse {
        subject_user_id: request.subject_user_id,
        tenant_id: ctx.tenant_id,
        archive_uri: "inline:privacy-export.tgz".to_string(),
        file_count: usize_to_u64(counts.len().saturating_add(3)),
        counts: counts
            .into_iter()
            .map(|(key, value)| (key.to_string(), usize_to_u64(value)))
            .collect(),
        manifest,
        archive_base64: Some(BASE64_STANDARD.encode(archive)),
    })
}

/// Runs privacy erasure after authz and approval-token verification have completed.
pub async fn run_privacy_erase(
    ctx: PrivacyEraseContext,
) -> Result<PrivacyEraseResponse, HandlerError> {
    if ctx.reason.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "--reason is required").into());
    }
    let linked_policy = match ctx
        .contact_erasure_scope
        .unwrap_or(ContactErasureScope::SpecifiedContact)
    {
        ContactErasureScope::SpecifiedContact => ContactLinkedSubjectPolicy::SpecifiedOnly,
        ContactErasureScope::SpecifiedAndLinkedContacts => {
            ContactLinkedSubjectPolicy::IncludeVerifiedLinks
        }
    };
    let resolved = resolve_privacy_subjects(
        &ctx.pool,
        ctx.tenant_id.0,
        Some(&WorkspaceId::new(ctx.workspace_id.clone())),
        &UserId::new(ctx.subject_user_id.clone()),
        linked_policy,
    )
    .await?;
    validate_contact_erasure_scope(&ctx, resolved.kind)?;
    let candidate_groups = enumerate_subject_erase_candidates(&ctx, &resolved.subjects).await?;
    let candidates = flatten_erase_candidates(&candidate_groups);

    if ctx.dry_run {
        return Ok(erase_response(&ctx, &candidates, 0, 0));
    }

    consume_approval_jti(&ctx.pool, &ctx.claims).await?;
    let pii_vault_erased = erase_pii_vault_subjects(&ctx, &resolved.subjects).await?;

    if candidates.is_empty() {
        return Ok(erase_response(&ctx, &candidates, 0, pii_vault_erased));
    }

    let mut erased_count = 0usize;
    for (subject, subject_candidates) in candidate_groups {
        if subject_candidates.is_empty() {
            continue;
        }
        erased_count = erased_count.saturating_add(
            hard_purge_erase_candidates(
                &ctx.pool,
                &graph_erasure_audit(&ctx, &subject),
                &subject_candidates,
            )
            .await
            .map_err(handler_error)?,
        );
    }

    Ok(erase_response(
        &ctx,
        &candidates,
        erased_count,
        pii_vault_erased,
    ))
}

/// Records an approval-token JTI and rejects token replay.
pub async fn consume_approval_jti(
    pool: &PgPool,
    claims: &ApprovalClaims,
) -> Result<(), HandlerError> {
    let expires_at = Utc
        .timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| TerminalError::new_with_code(400, "approval token exp is out of range"))?;
    let inserted = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (jti) DO NOTHING
        RETURNING jti
        "#,
    )
    .bind(&claims.jti)
    .bind(&claims.op)
    .bind(&claims.subject_user_id)
    .bind(&claims.sub)
    .bind(serde_json::to_value(claims).map_err(handler_error)?)
    .bind(expires_at)
    .fetch_optional(pool)
    .await
    .map_err(handler_error)?;
    ensure_jti_inserted(inserted.as_deref())
}

/// Ensures an approval-token JTI insert actually inserted a new row.
pub fn ensure_jti_inserted(inserted: Option<&str>) -> Result<(), HandlerError> {
    if inserted.is_some() {
        Ok(())
    } else {
        Err(TerminalError::new_with_code(409, "approval token replayed").into())
    }
}

fn validate_claims(
    claims: &ApprovalClaims,
    expected_op: &str,
    subject_user_id: &str,
    workspace: Option<&str>,
) -> Result<(), HandlerError> {
    if claims.sub.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "approval token missing sub").into());
    }
    if claims.jti.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "approval token missing jti").into());
    }
    if claims.op != expected_op {
        return Err(TerminalError::new_with_code(
            400,
            format!("approval token op must be `{expected_op}`"),
        )
        .into());
    }
    if claims.subject_user_id != subject_user_id {
        return Err(
            TerminalError::new_with_code(400, "approval token subject_user_id mismatch").into(),
        );
    }
    if !claims.has_platform_admin_role() {
        return Err(TerminalError::new_with_code(
            403,
            "approval token requires platform_admin role",
        )
        .into());
    }
    let now = Utc::now().timestamp();
    if claims.exp <= now {
        return Err(TerminalError::new_with_code(401, "approval token expired").into());
    }
    if let Some(token_workspace) = claims.workspace_id.as_deref()
        && Some(token_workspace) != workspace
    {
        return Err(
            TerminalError::new_with_code(400, "approval token workspace_id mismatch").into(),
        );
    }
    Ok(())
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, HandlerError> {
    URL_SAFE_NO_PAD.decode(value).map_err(|error| {
        TerminalError::new_with_code(400, format!("invalid base64url value: {error}")).into()
    })
}

fn decode_key_material(raw: &str) -> Result<Vec<u8>, HandlerError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TerminalError::new_with_code(400, "key material is empty").into());
    }
    if trimmed.len().is_multiple_of(2)
        && trimmed
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return hex::decode(trimmed).map_err(handler_error);
    }
    BASE64_STANDARD
        .decode(trimmed)
        .or_else(|_| URL_SAFE_NO_PAD.decode(trimmed))
        .map_err(handler_error)
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContactLinkedSubjectPolicy {
    SpecifiedOnly,
    IncludeVerifiedLinks,
}

async fn resolve_privacy_subjects(
    pool: &PgPool,
    tenant_id: Uuid,
    workspace_id: Option<&WorkspaceId>,
    requested_subject_user_id: &UserId,
    linked_policy: ContactLinkedSubjectPolicy,
) -> Result<ResolvedPrivacySubjects, HandlerError> {
    let parsed = parse_privacy_subject_id(requested_subject_user_id)?;
    let contact_row = load_privacy_contact_row(pool, tenant_id, workspace_id, parsed.uuid).await?;
    let Some(contact_row) = contact_row else {
        if parsed.is_contact_prefixed {
            return Err(TerminalError::new_with_code(404, "contact not found").into());
        }
        return Ok(ResolvedPrivacySubjects {
            kind: PrivacySubjectKind::User,
            effective_workspace: workspace_id.map(ToString::to_string),
            subjects: vec![PrivacySubject::primary(
                requested_subject_user_id.to_string(),
                parsed.uuid,
            )],
        });
    };

    let mut subjects = vec![PrivacySubject::primary(
        format!("{CONTACT_SUBJECT_PREFIX}{}", contact_row.id),
        contact_row.id,
    )];
    if contact_row.state == "verified"
        && linked_policy == ContactLinkedSubjectPolicy::IncludeVerifiedLinks
    {
        let linked =
            load_linked_contact_ids(pool, tenant_id, &contact_row.workspace_id, contact_row.id)
                .await?;
        subjects.extend(linked.into_iter().map(PrivacySubject::linked_contact));
    }

    Ok(ResolvedPrivacySubjects {
        kind: PrivacySubjectKind::Contact,
        effective_workspace: Some(contact_row.workspace_id),
        subjects,
    })
}

#[derive(Debug, Clone, Copy)]
struct ParsedPrivacySubjectId {
    uuid: Uuid,
    is_contact_prefixed: bool,
}

fn parse_privacy_subject_id(
    subject_user_id: &UserId,
) -> Result<ParsedPrivacySubjectId, HandlerError> {
    let raw = subject_user_id.as_str();
    let (value, is_contact_prefixed) = raw
        .strip_prefix(CONTACT_SUBJECT_PREFIX)
        .map_or((raw, false), |value| (value, true));
    let uuid = Uuid::parse_str(value).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("subject_user_id must be a UUID-backed user id: {error}"),
        )
    })?;
    Ok(ParsedPrivacySubjectId {
        uuid,
        is_contact_prefixed,
    })
}

#[derive(Debug, Clone)]
struct PrivacyContactRow {
    id: Uuid,
    workspace_id: String,
    state: String,
}

async fn load_privacy_contact_row(
    pool: &PgPool,
    tenant_id: Uuid,
    workspace_id: Option<&WorkspaceId>,
    contact_id: Uuid,
) -> Result<Option<PrivacyContactRow>, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, workspace_id, state
        FROM contacts
        WHERE id = $1
          AND tenant_id = $2
          AND ($3::text IS NULL OR workspace_id = $3)
        "#,
    )
    .bind(contact_id)
    .bind(tenant_id)
    .bind(workspace_id.map(WorkspaceId::as_str))
    .fetch_optional(pool)
    .await
    .map_err(|error| db_handler_error("load privacy contact subject", error))?;
    row.map(|row| {
        Ok(PrivacyContactRow {
            id: row
                .try_get::<Uuid, _>("id")
                .map_err(|error| db_handler_error("read privacy contact id", error))?,
            workspace_id: row
                .try_get::<String, _>("workspace_id")
                .map_err(|error| db_handler_error("read privacy contact workspace", error))?,
            state: row
                .try_get::<String, _>("state")
                .map_err(|error| db_handler_error("read privacy contact state", error))?,
        })
    })
    .transpose()
}

async fn load_linked_contact_ids(
    pool: &PgPool,
    tenant_id: Uuid,
    workspace_id: &str,
    contact_id: Uuid,
) -> Result<Vec<Uuid>, HandlerError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM contacts
        WHERE canonical_contact_id = $1
          AND tenant_id = $2
          AND workspace_id = $3
        ORDER BY merged_at NULLS LAST, updated_at DESC, id
        "#,
    )
    .bind(contact_id)
    .bind(tenant_id)
    .bind(workspace_id)
    .fetch_all(pool)
    .await
    .map_err(|error| db_handler_error("load linked privacy contacts", error))
}

fn validate_contact_erasure_scope(
    ctx: &PrivacyEraseContext,
    kind: PrivacySubjectKind,
) -> Result<(), HandlerError> {
    match (kind, ctx.contact_erasure_scope) {
        (PrivacySubjectKind::Contact, Some(_)) => Ok(()),
        (PrivacySubjectKind::Contact, None) => Err(TerminalError::new_with_code(
            400,
            "contact_erasure_scope is required for contact erasure",
        )
        .into()),
        (PrivacySubjectKind::User, None) => Ok(()),
        (PrivacySubjectKind::User, Some(_)) => Err(TerminalError::new_with_code(
            400,
            "contact_erasure_scope only applies to contact subjects",
        )
        .into()),
    }
}

async fn enumerate_subject_erase_candidates(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
) -> Result<Vec<(PrivacySubject, Vec<EraseCandidate>)>, HandlerError> {
    let mut groups = Vec::with_capacity(subjects.len());
    for subject in subjects {
        let candidates = enumerate_erase_candidates(&ctx.pool, &ctx.workspace_id, &subject.user_id)
            .await
            .map_err(handler_error)?;
        groups.push((subject.clone(), candidates));
    }
    Ok(groups)
}

#[derive(Debug, Clone)]
struct SubjectEraseCandidate {
    subject: PrivacySubject,
    candidate: EraseCandidate,
}

fn flatten_erase_candidates(
    groups: &[(PrivacySubject, Vec<EraseCandidate>)],
) -> Vec<SubjectEraseCandidate> {
    groups
        .iter()
        .flat_map(|(subject, candidates)| {
            candidates
                .iter()
                .cloned()
                .map(|candidate| SubjectEraseCandidate {
                    subject: subject.clone(),
                    candidate,
                })
        })
        .collect()
}

async fn erase_pii_vault_subjects(
    ctx: &PrivacyEraseContext,
    subjects: &[PrivacySubject],
) -> Result<u64, HandlerError> {
    let Some(secret) = ctx.pii_vault_secret.clone() else {
        tracing::warn!(
            workspace_id = %ctx.workspace_id,
            subject_user_id = %ctx.subject_user_id,
            "skipping PII vault erase because no PII vault secret is configured"
        );
        return Ok(0);
    };

    let vault = PiiVault::with_pool(ctx.pool.clone(), secret, "privacy-erase");
    let mut erased = 0u64;
    for subject in subjects {
        let subject_pseudonym = vault
            .subject_pseudonym(&subject.user_id)
            .map_err(handler_error)?;
        erased = erased.saturating_add(
            vault
                .erase_subject(&ctx.workspace_id, &subject_pseudonym)
                .await
                .map_err(handler_error)?,
        );
    }
    Ok(erased)
}

fn graph_erasure_audit(ctx: &PrivacyEraseContext, subject: &PrivacySubject) -> GraphErasureAudit {
    GraphErasureAudit {
        workspace_id: ctx.workspace_id.clone(),
        subject_user: subject.target_uid,
        subject_user_id: subject.user_id.clone(),
        reason: ctx.reason.clone(),
        approver_id: ctx.claims.sub.clone(),
        approval_token_jti: ctx.claims.jti.clone(),
    }
}

fn erase_response(
    ctx: &PrivacyEraseContext,
    candidates: &[SubjectEraseCandidate],
    erased_count: usize,
    pii_vault_erased: u64,
) -> PrivacyEraseResponse {
    PrivacyEraseResponse {
        tenant_id: ctx.tenant_id,
        subject_user_id: UserId::new(ctx.subject_user_id.clone()),
        candidate_count: usize_to_u64(candidates.len()),
        erased_count: usize_to_u64(erased_count),
        pii_vault_erased,
        dry_run: ctx.dry_run,
        sample: candidates
            .iter()
            .take(ERASE_SAMPLE_LIMIT)
            .map(|candidate| {
                json!({
                    "uid": candidate.candidate.uid,
                    "label": candidate.candidate.label,
                    "name": candidate.candidate.name,
                    "pii_class": candidate.candidate.pii_class,
                    "privacy_subject_user_id": candidate.subject.user_id.as_str(),
                    "privacy_subject_provenance": candidate.subject.provenance.as_str(),
                })
            })
            .collect(),
    }
}

/// Collects privacy export data sections before README, audit, changelog, and manifest generation.
pub async fn collect_privacy_export_data_sections(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<BTreeMap<&'static str, usize>, HandlerError> {
    let mut counts = BTreeMap::new();
    counts.insert("facts", collect_facts(ctx, export_dir).await?);
    counts.insert("entities", collect_entities(ctx, export_dir).await?);
    counts.insert(
        "relationships",
        collect_relationships(ctx, export_dir).await?,
    );
    counts.insert("embeddings", collect_embeddings(ctx, export_dir).await?);
    counts.insert("skills", collect_skills(ctx, export_dir).await?);
    Ok(counts)
}

async fn collect_facts(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    collect_nodes(
        ctx,
        export_dir.join("facts.jsonl"),
        &["Fact", "Lesson", "Decision", "Incident"],
    )
    .await
}

async fn collect_entities(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    collect_nodes(
        ctx,
        export_dir.join("entities.jsonl"),
        &["Entity", "Concept", "Source"],
    )
    .await
}

async fn collect_nodes(
    ctx: &PrivacyExportContext,
    path: PathBuf,
    labels: &[&str],
) -> Result<usize, HandlerError> {
    let label_filter = labels
        .iter()
        .map(|label| (*label).to_string())
        .collect::<Vec<_>>();
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'uid', uid,
                    'label', label,
                    'workspace_id', workspace_id,
                    'user_id', user_id,
                    'scope', scope,
                    'name', name,
                    'properties_summary', properties_summary,
                    'pii_class', pii_class,
                    'confidence', confidence,
                    'valid_from', valid_from,
                    'valid_to', valid_to,
                    'created_at', created_at,
                    'last_accessed_at', last_accessed_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $4
                )
                FROM moa.node_index
                WHERE valid_to IS NULL
                  AND label = ANY($3)
                  AND ($1::text IS NULL OR workspace_id = $1)
                  AND (
                      user_id = $2
                      OR properties_summary->>'user_id' = $2
                      OR properties_summary::text LIKE ('%' || $2 || '%')
                  )
                ORDER BY workspace_id NULLS FIRST, label, name, uid
                "#,
            )
            .bind(ctx.workspace.as_deref())
            .bind(&subject.user_id)
            .bind(label_filter.clone())
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(path, &rows).await
}

async fn collect_relationships(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'change_id', change_id,
                    'workspace_id', workspace_id,
                    'user_id', user_id,
                    'scope', scope,
                    'actor_id', actor_id,
                    'actor_kind', actor_kind,
                    'op', op,
                    'target_kind', target_kind,
                    'target_label', target_label,
                    'target_uid', target_uid,
                    'payload', payload,
                    'pii_class', pii_class,
                    'audit_metadata', audit_metadata,
                    'cause_change_id', cause_change_id,
                    'created_at', created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.graph_changelog
                WHERE target_kind = 'edge'
                  AND ($1::text IS NULL OR workspace_id = $1)
                  AND (
                      user_id = $2
                      OR actor_id = $2
                      OR payload::text LIKE ('%' || $2 || '%')
                      OR audit_metadata->>'subject_user_id' = $2
                  )
                ORDER BY created_at, change_id
                "#,
            )
            .bind(ctx.workspace.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("relationships.jsonl"), &rows).await
}

async fn collect_embeddings(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'uid', e.uid,
                    'workspace_id', e.workspace_id,
                    'user_id', e.user_id,
                    'scope', e.scope,
                    'label', e.label,
                    'pii_class', e.pii_class,
                    'embedding_model', e.embedding_model,
                    'embedding_model_version', e.embedding_model_version,
                    'embedding', (e.embedding::text)::jsonb,
                    'valid_to', e.valid_to,
                    'created_at', e.created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.embeddings e
                JOIN moa.node_index n ON n.uid = e.uid
                WHERE e.valid_to IS NULL
                  AND n.valid_to IS NULL
                  AND ($1::text IS NULL OR e.workspace_id = $1)
                  AND (
                      e.user_id = $2
                      OR n.user_id = $2
                      OR n.properties_summary->>'user_id' = $2
                      OR n.properties_summary::text LIKE ('%' || $2 || '%')
                  )
                ORDER BY e.workspace_id NULLS FIRST, e.label, e.uid
                "#,
            )
            .bind(ctx.workspace.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("embeddings.jsonl"), &rows).await
}

async fn collect_skills(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'artifact_uid', a.artifact_uid,
                    'revision_uid', r.revision_uid,
                    'workspace_id', a.workspace_id,
                    'user_id', a.user_id,
                    'scope', a.scope,
                    'name', a.name,
                    'description', a.description,
                    'tags', a.tags,
                    'definition', r.definition,
                    'canonical_hash_hex', encode(r.canonical_hash, 'hex'),
                    'source_format', r.source_format,
                    'source_text_base64', encode(r.source_text, 'base64'),
                    'status', r.status,
                    'version', r.version,
                    'published_at', r.published_at,
                    'valid_to', r.valid_to,
                    'created_at', r.created_at,
                    'updated_at', r.updated_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3,
                    'files', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'path', f.path,
                            'content_base64', encode(f.content, 'base64'),
                            'content_sha256_hex', encode(f.content_sha256, 'hex'),
                            'content_type', f.content_type,
                            'executable', f.executable,
                            'file_size_bytes', f.file_size_bytes
                        ) ORDER BY f.path)
                        FROM moa.artifact_file f
                        WHERE f.revision_uid = r.revision_uid
                    ), '[]'::jsonb)
                )
                FROM moa.artifact a
                JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
                WHERE a.valid_to IS NULL
                  AND r.valid_to IS NULL
                  AND a.kind = 'skill'
                  AND r.status = 'published'
                  AND ($1::text IS NULL OR a.workspace_id = $1)
                  AND (
                      a.user_id = $2
                      OR a.description LIKE ('%' || $2 || '%')
                      OR r.definition::text LIKE ('%' || $2 || '%')
                      OR encode(r.source_text, 'escape') LIKE ('%' || $2 || '%')
                      OR EXISTS (
                          SELECT 1
                          FROM moa.artifact_file f
                          WHERE f.revision_uid = r.revision_uid
                            AND encode(f.content, 'escape') LIKE ('%' || $2 || '%')
                      )
                  )
                ORDER BY a.workspace_id NULLS FIRST, a.scope, a.name, r.version
                "#,
            )
            .bind(ctx.workspace.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("skills.jsonl"), &rows).await
}

async fn collect_changelog(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'change_id', change_id,
                    'workspace_id', workspace_id,
                    'user_id', user_id,
                    'scope', scope,
                    'actor_id', actor_id,
                    'actor_kind', actor_kind,
                    'op', op,
                    'target_kind', target_kind,
                    'target_label', target_label,
                    'target_uid', target_uid,
                    'payload', payload,
                    'redaction_marker', redaction_marker,
                    'pii_class', pii_class,
                    'audit_metadata', audit_metadata,
                    'cause_change_id', cause_change_id,
                    'created_at', created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.graph_changelog
                WHERE ($1::text IS NULL OR workspace_id = $1)
                  AND (
                      user_id = $2
                      OR actor_id = $2
                      OR target_uid::text = $2
                      OR payload::text LIKE ('%' || $2 || '%')
                      OR audit_metadata->>'subject_user_id' = $2
                  )
                ORDER BY created_at, change_id
                "#,
            )
            .bind(ctx.workspace.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("changelog.jsonl"), &rows).await
}

async fn begin_audited_read(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, HandlerError> {
    let mut tx = pool.begin().await.map_err(handler_error)?;
    sqlx::query("SET LOCAL ROLE moa_auditor")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    Ok(tx)
}

async fn write_jsonl(path: PathBuf, rows: &[Value]) -> Result<usize, HandlerError> {
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(handler_error)?;
    for row in rows {
        file.write_all(
            serde_json::to_string(row)
                .map_err(handler_error)?
                .as_bytes(),
        )
        .await
        .map_err(handler_error)?;
        file.write_all(b"\n").await.map_err(handler_error)?;
    }
    file.flush().await.map_err(handler_error)?;
    Ok(rows.len())
}

/// Writes the privacy export README file.
pub async fn write_export_readme(
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
    export_dir: &Path,
) -> Result<(), HandlerError> {
    let mut lines = Vec::new();
    lines.push("# MOA subject access export".to_string());
    lines.push(String::new());
    lines.push(format!("Created at: {}", Utc::now().to_rfc3339()));
    lines.push(format!("Subject user id: {}", ctx.subject_user_id));
    lines.push(format!(
        "Workspace: {}",
        ctx.workspace.as_deref().unwrap_or("all")
    ));
    lines.push("Included subjects:".to_string());
    for subject in &ctx.subjects {
        lines.push(format!(
            "- {} ({})",
            subject.user_id,
            subject.provenance.as_str()
        ));
    }
    lines.push(format!("Reason: {}", ctx.reason));
    lines.push(String::new());
    lines.push("This archive contains MOA graph memory, skills, addenda, embeddings, and audit rows attributable to the subject user for a GDPR Article 15 subject access request.".to_string());
    lines.push("MOA stores redacted graph-memory text after ingestion. This export does not decrypt or restore original PHI; it emits the persisted redacted data as stored.".to_string());
    lines.push("The archive may still contain quasi-identifiers and should be delivered only through an approved secure channel.".to_string());
    lines.push(String::new());
    lines.push("## Row counts".to_string());
    for (name, count) in counts {
        lines.push(format!("- {name}: {count}"));
    }
    lines.push(String::new());
    lines.push("## Manifest verification".to_string());
    lines.push("Verify `manifest.sig` as an Ed25519 signature over the exact bytes of `manifest.json` using the ops export public key recorded in the manifest.".to_string());
    lines.push(String::new());
    lines.push(
        "Contact the MOA platform operations team for follow-up questions or corrections."
            .to_string(),
    );
    lines.push(String::new());

    tokio::fs::write(export_dir.join("README.md"), lines.join("\n"))
        .await
        .map_err(handler_error)?;
    Ok(())
}

async fn emit_export_audit(
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<(), HandlerError> {
    let mut tx = ctx.pool.begin().await.map_err(handler_error)?;
    let scope = if ctx.workspace.is_some() {
        "workspace"
    } else {
        "global"
    };
    let file_count = counts.len().saturating_add(4);
    write_and_bump(
        &mut tx,
        ChangelogRecord {
            workspace_id: ctx.workspace.clone(),
            user_id: None,
            scope: scope.to_string(),
            actor_id: Some(ctx.claims.sub.clone()),
            actor_kind: "admin".to_string(),
            op: "export".to_string(),
            target_kind: "user".to_string(),
            target_label: "User".to_string(),
            target_uid: ctx.subject_user,
            payload: json!({
                "reason": ctx.reason,
                "subject_user_id": ctx.subject_user_id,
                "subjects": privacy_subjects_json(&ctx.subjects),
                "workspace": ctx.workspace.as_deref(),
                "artifact_counts": counts,
                "files": file_count,
            }),
            redaction_marker: None,
            pii_class: "phi".to_string(),
            audit_metadata: Some(json!({
                "approval_token_jti": ctx.claims.jti.as_str(),
                "approval_token_sub": ctx.claims.sub.as_str(),
                "subject_user_id": ctx.subject_user_id,
                "subjects": privacy_subjects_json(&ctx.subjects),
                "op": "export",
            })),
            cause_change_id: None,
        },
    )
    .await
    .map_err(handler_error)?;
    tx.commit().await.map_err(handler_error)?;
    Ok(())
}

fn privacy_subjects_json(subjects: &[PrivacySubject]) -> Value {
    Value::Array(
        subjects
            .iter()
            .map(|subject| {
                json!({
                    "user_id": subject.user_id.as_str(),
                    "target_uid": subject.target_uid,
                    "provenance": subject.provenance.as_str(),
                })
            })
            .collect(),
    )
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    version: u8,
    created_at: String,
    subject_user_id: &'a str,
    subjects: Vec<ManifestSubject<'a>>,
    workspace: Option<&'a str>,
    encryption: &'static str,
    signature: ManifestSignature<'a>,
    files: Vec<ManifestFile>,
    counts: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Serialize)]
struct ManifestSubject<'a> {
    user_id: &'a str,
    provenance: &'static str,
}

#[derive(Debug, Serialize)]
struct ManifestSignature<'a> {
    algorithm: &'static str,
    signature_file: &'static str,
    key_id: &'a str,
    public_key_hex: String,
}

#[derive(Debug, Serialize)]
struct ManifestFile {
    name: String,
    size: u64,
    sha256: String,
    blake3: String,
}

/// Writes and signs `manifest.json`, returning the manifest JSON value.
pub async fn write_manifest(
    export_dir: &Path,
    signer: &Ed25519ManifestSigner,
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<Value, HandlerError> {
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(export_dir)
        .await
        .map_err(handler_error)?;
    while let Some(entry) = entries.next_entry().await.map_err(handler_error)? {
        let path = entry.path();
        if !entry.file_type().await.map_err(handler_error)?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "manifest.json" || name == "manifest.sig" {
            continue;
        }
        let bytes = tokio::fs::read(&path).await.map_err(handler_error)?;
        files.push(ManifestFile {
            name: name.to_string(),
            size: usize_to_u64(bytes.len()),
            sha256: sha256_hex(&bytes),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));

    let manifest = Manifest {
        version: 1,
        created_at: Utc::now().to_rfc3339(),
        subject_user_id: &ctx.subject_user_id,
        subjects: ctx
            .subjects
            .iter()
            .map(|subject| ManifestSubject {
                user_id: subject.user_id.as_str(),
                provenance: subject.provenance.as_str(),
            })
            .collect(),
        workspace: ctx.workspace.as_deref(),
        encryption: "none",
        signature: ManifestSignature {
            algorithm: "Ed25519",
            signature_file: "manifest.sig",
            key_id: signer.key_id(),
            public_key_hex: signer.public_key_hex(),
        },
        files,
        counts: counts.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(handler_error)?;
    tokio::fs::write(export_dir.join("manifest.json"), &manifest_bytes)
        .await
        .map_err(handler_error)?;
    tokio::fs::write(
        export_dir.join("manifest.sig"),
        signer.sign(&manifest_bytes),
    )
    .await
    .map_err(handler_error)?;
    serde_json::from_slice(&manifest_bytes).map_err(handler_error)
}

/// Creates a gzipped tar archive from an export directory and returns its bytes.
pub async fn finalize_archive_to_bytes(
    export_dir: &Path,
    pgp_recipient: Option<&str>,
) -> Result<Vec<u8>, HandlerError> {
    let parent = export_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let target = parent.join("subject.tgz");
    let export_dir_for_archive = export_dir.to_path_buf();
    let target_for_archive = target.clone();
    tokio::task::spawn_blocking(move || -> Result<(), HandlerError> {
        let file = std::fs::File::create(&target_for_archive).map_err(handler_error)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        archive
            .append_dir_all("export", &export_dir_for_archive)
            .map_err(handler_error)?;
        let encoder = archive.into_inner().map_err(handler_error)?;
        encoder.finish().map_err(handler_error)?;
        Ok(())
    })
    .await
    .map_err(handler_error)??;

    if let Some(recipient) = pgp_recipient {
        let encrypted = encrypt_with_gpg(&target, &parent, recipient).await?;
        return tokio::fs::read(encrypted).await.map_err(handler_error);
    }

    tokio::fs::read(target).await.map_err(handler_error)
}

async fn encrypt_with_gpg(
    target: &Path,
    parent: &Path,
    recipient: &str,
) -> Result<PathBuf, HandlerError> {
    let recipient_path = parent.join("recipient.asc");
    tokio::fs::write(&recipient_path, recipient)
        .await
        .map_err(handler_error)?;
    let output = parent.join("subject.tgz.gpg");
    let status = Command::new("gpg")
        .arg("--batch")
        .arg("--yes")
        .arg("--encrypt")
        .arg("--recipient-file")
        .arg(&recipient_path)
        .arg("--output")
        .arg(&output)
        .arg(target)
        .status()
        .await
        .map_err(handler_error)?;
    if !status.success() {
        return Err(
            TerminalError::new(format!("gpg encryption failed with status {status}")).into(),
        );
    }
    Ok(output)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn parse_subject_uuid(subject_user_id: &UserId) -> Result<Uuid, HandlerError> {
    parse_privacy_subject_id(subject_user_id).map(|parsed| parsed.uuid)
}

async fn create_temp_dir(prefix: &str) -> Result<PathBuf, HandlerError> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::now_v7()));
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(handler_error)?;
    Ok(path)
}

async fn cleanup_temp_dir(path: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        tracing::warn!(path = %path.display(), %error, "failed to remove privacy export staging directory");
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn db_handler_error(context: &'static str, error: sqlx::Error) -> HandlerError {
    TerminalError::new(format!("{context}: {error}")).into()
}
