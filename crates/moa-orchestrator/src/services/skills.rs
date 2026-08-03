//! Restate service for cloud-owned skill export and listing.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_authz_schema::Relation;
use moa_core::{types::action_policy::ActionRuleScope, types::identifiers::TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_skills::registry::{Skill, SkillRegistry, StoredSkillPackage};
use moa_wire::skills::{
    SkillExportRequest, SkillExportResponse, SkillListRequest, SkillListResponse,
    SkillPackageDocument, SkillPackageDocumentFile, SkillSummary,
};
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::AuthzEnforcer;
use crate::workflows::errors::moa_error_to_status_handler_error;

/// Restate service surface for protected skill operations.
#[restate_sdk::service]
#[name = "Skills"]
pub trait Skills {
    /// Exports visible tenant skills after a tenant operator check.
    async fn export(
        request: Json<SkillExportRequest>,
    ) -> Result<Json<SkillExportResponse>, HandlerError>;

    /// Lists visible tenant skills after a tenant operator check.
    async fn list(request: Json<SkillListRequest>)
    -> Result<Json<SkillListResponse>, HandlerError>;
}

/// Concrete skill service implementation.
#[derive(Clone)]
pub struct SkillsImpl {
    pool: sqlx::PgPool,
    authz: AuthzEnforcer,
}

impl SkillsImpl {
    /// Creates the skills adapter with its artifact, skill, and knowledge pool.
    #[must_use]
    pub fn new(pool: sqlx::PgPool, authz: AuthzEnforcer) -> Self {
        Self { pool, authz }
    }
}

impl Skills for SkillsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<SkillExportRequest>,
    ) -> Result<Json<SkillExportResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Skills", "export");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { export_inner(pool, request).await.map(Json::from) })
            .name("skills_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<SkillListRequest>,
    ) -> Result<Json<SkillListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Skills", "list");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_inner(pool, request).await.map(Json::from) })
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

async fn export_inner(
    pool: sqlx::PgPool,
    request: SkillExportRequest,
) -> Result<SkillExportResponse, HandlerError> {
    let tenant_id = request.tenant_id;
    let scope = ActionRuleScope::Tenant { tenant_id };
    let registry = skill_registry(pool);
    let packages = registry
        .load_packages_for_scope(&scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let packages = packages
        .into_iter()
        .map(skill_package_document_from_stored)
        .collect();
    Ok(SkillExportResponse {
        tenant_id,
        packages,
    })
}

async fn list_inner(
    pool: sqlx::PgPool,
    request: SkillListRequest,
) -> Result<SkillListResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let registry = skill_registry(pool);
    let skills = registry
        .load_for_scope(&scope)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let skills = skills
        .into_iter()
        .map(skill_summary_from_skill)
        .collect::<Result<Vec<_>, HandlerError>>()?;
    Ok(SkillListResponse { skills })
}

fn skill_registry(pool: sqlx::PgPool) -> SkillRegistry {
    SkillRegistry::new(pool)
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

fn memory_scope_from_skill(skill: &Skill) -> Result<ActionRuleScope, HandlerError> {
    skill_scope_from_stored_parts(&skill.scope, skill.tenant_id)
}
