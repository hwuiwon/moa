//! Restate service for cloud-owned skill import, export, listing, and bootstrap requests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::wire::skills::{
    SkillExportRequest, SkillExportResponse, SkillImportRequest, SkillImportResponse,
    SkillListRequest, SkillListResponse, SkillPackageDocument, SkillPackageDocumentFile,
    SkillSummary,
};
use moa_core::{ActionRuleScope, MoaError, TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry, StoredSkillPackage};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Restate service surface for protected skill operations.
#[restate_sdk::service]
#[name = "Skills"]
pub trait Skills {
    /// Exports visible tenant skills after a tenant operator check.
    async fn export(
        request: Json<SkillExportRequest>,
    ) -> Result<Json<SkillExportResponse>, HandlerError>;

    /// Imports tenant-scoped skill packages after the matching authz check.
    async fn import(
        request: Json<SkillImportRequest>,
    ) -> Result<Json<SkillImportResponse>, HandlerError>;

    /// Lists visible tenant skills after a tenant operator check.
    async fn list(request: Json<SkillListRequest>)
    -> Result<Json<SkillListResponse>, HandlerError>;
}

/// Concrete skill service implementation.
#[derive(Clone, Default)]
pub struct SkillsImpl;

impl Skills for SkillsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<SkillExportRequest>,
    ) -> Result<Json<SkillExportResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "export");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        Ok(ctx
            .run(|| async move { export_inner(request).await.map(Json::from) })
            .name("skills_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn import(
        &self,
        ctx: Context<'_>,
        request: Json<SkillImportRequest>,
    ) -> Result<Json<SkillImportResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "import");
        let request = request.into_inner();
        let scope = authorized_import_scope(&ctx, request.scope).await?;
        let packages = request.packages;

        Ok(ctx
            .run(|| async move { import_inner(scope, packages).await.map(Json::from) })
            .name("skills_import")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<SkillListRequest>,
    ) -> Result<Json<SkillListResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "list");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        Ok(ctx
            .run(|| async move { list_inner(request).await.map(Json::from) })
            .name("skills_list")
            .await?)
    }
}

/// Converts a registry skill row into a public API summary.
pub fn skill_summary_from_skill(skill: Skill) -> Result<SkillSummary, HandlerError> {
    Ok(SkillSummary {
        skill_uid: skill.skill_uid,
        scope: memory_scope_from_skill(&skill)?,
        version: skill.version,
        name: skill.name,
        description: skill.description,
        tags: skill.tags,
        package_hash: hex::encode(skill.package_hash),
        skill_md_hash: hex::encode(skill.skill_md_hash),
        file_count: skill.file_count,
        total_size_bytes: skill.total_size_bytes,
        created_at: skill.created_at,
        updated_at: skill.updated_at,
    })
}

async fn export_inner(request: SkillExportRequest) -> Result<SkillExportResponse, HandlerError> {
    let tenant_id = request.tenant_id;
    let scope = ActionRuleScope::Tenant { tenant_id };
    let registry = skill_registry();
    let packages = registry
        .load_packages_for_scope(&scope)
        .await
        .map_err(skill_handler_error)?;
    let packages = packages
        .into_iter()
        .map(skill_package_document_from_stored)
        .collect();
    Ok(SkillExportResponse {
        tenant_id,
        packages,
    })
}

async fn import_inner(
    scope: ActionRuleScope,
    packages: Vec<SkillPackageDocument>,
) -> Result<SkillImportResponse, HandlerError> {
    let registry = skill_registry();
    let mut imported = 0_u64;
    for package in packages {
        let files = decode_skill_package_files(package.files)?;
        let skill = NewSkill::from_package(scope, SkillPackage::new(files));
        registry
            .upsert_by_name(skill)
            .await
            .map_err(skill_handler_error)?;
        imported = imported.saturating_add(1);
    }
    Ok(SkillImportResponse { scope, imported })
}

async fn list_inner(request: SkillListRequest) -> Result<SkillListResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = skill_registry();
    let skills = registry
        .load_for_scope(&scope)
        .await
        .map_err(skill_handler_error)?;
    let skills = skills
        .into_iter()
        .map(skill_summary_from_skill)
        .collect::<Result<Vec<_>, HandlerError>>()?;
    Ok(SkillListResponse { skills })
}

fn skill_registry() -> SkillRegistry {
    SkillRegistry::new(OrchestratorCtx::current_graph_pool())
}

async fn authorized_import_scope(
    ctx: &impl RequestHeaders,
    scope: ActionRuleScope,
) -> Result<ActionRuleScope, HandlerError> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => {
            authorize_tenant(ctx, tenant_id, Relation::Operator).await?;
        }
    }
    Ok(scope)
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .map_err(translate_authz_error)?;
    Ok(identity)
}

fn reject_user_scoped_skill() -> HandlerError {
    TerminalError::new_with_code(500, "contact-scoped skill rows are not supported").into()
}

fn skill_scope_from_stored_parts(
    scope: &str,
    tenant_id: Option<TenantId>,
) -> Result<ActionRuleScope, HandlerError> {
    match scope {
        "tenant" => tenant_id
            .map(|tenant_id| ActionRuleScope::Tenant { tenant_id })
            .ok_or_else(|| {
                TerminalError::new_with_code(500, "tenant skill row missing tenant id").into()
            }),
        "user" => Err(reject_user_scoped_skill()),
        other => {
            Err(TerminalError::new_with_code(500, format!("unknown skill scope `{other}`")).into())
        }
    }
}

fn skill_package_document_from_stored(stored: StoredSkillPackage) -> SkillPackageDocument {
    let skill = stored.skill;
    SkillPackageDocument {
        name: Some(skill.name),
        description: Some(skill.description),
        files: stored
            .files
            .into_iter()
            .map(|file| SkillPackageDocumentFile {
                path: file.path,
                content_base64: BASE64.encode(file.content),
                content_type: file.content_type,
                executable: file.executable,
            })
            .collect(),
        source_uri: None,
        metadata: serde_json::json!({
            "skill_uid": skill.skill_uid,
            "version": skill.version,
            "package_hash": hex::encode(skill.package_hash),
            "skill_md_hash": hex::encode(skill.skill_md_hash),
            "file_count": skill.file_count,
            "total_size_bytes": skill.total_size_bytes,
            "manifest": skill.manifest,
        }),
    }
}

fn decode_skill_package_files(
    files: Vec<SkillPackageDocumentFile>,
) -> Result<Vec<SkillPackageFile>, HandlerError> {
    files
        .into_iter()
        .map(|file| {
            let content = BASE64.decode(&file.content_base64).map_err(|error| {
                HandlerError::from(TerminalError::new_with_code(
                    400,
                    format!(
                        "skill package file `{}` content_base64 is invalid: {error}",
                        file.path
                    ),
                ))
            })?;
            Ok(SkillPackageFile {
                path: file.path,
                content,
                content_type: file.content_type,
                executable: file.executable,
            })
        })
        .collect()
}

fn memory_scope_from_skill(skill: &Skill) -> Result<ActionRuleScope, HandlerError> {
    skill_scope_from_stored_parts(&skill.scope, skill.tenant_id)
}

fn skill_handler_error(error: MoaError) -> HandlerError {
    match error {
        MoaError::ValidationError(_) | MoaError::SerializationError(_) | MoaError::Uuid(_) => {
            TerminalError::new_with_code(400, error.to_string()).into()
        }
        MoaError::Unsupported(_) | MoaError::NotImplemented(_) => {
            TerminalError::new_with_code(501, error.to_string()).into()
        }
        other if other.is_fatal() => TerminalError::new(other.to_string()).into(),
        other => HandlerError::from(other),
    }
}
