//! Restate service for cloud-owned skill import, export, listing, and
//! skill-backed procedure run lifecycle operations.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::registry::ArtifactRegistry;
use moa_authz_schema::Relation;
use moa_core::wire::procedures::{
    ProcedureCancelRequest, ProcedureCancelResponse, ProcedureNodeRunSummary,
    ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse, ProcedureRunRequest,
    ProcedureRunResponse, ProcedureRunStatus, ProcedureSignalRequest, ProcedureSignalResponse,
    ProcedureStatusRequest,
};
use moa_core::wire::skills::{
    SkillExportRequest, SkillExportResponse, SkillImportRequest, SkillImportResponse,
    SkillListRequest, SkillListResponse, SkillPackageDocument, SkillPackageDocumentFile,
    SkillSummary,
};
use moa_core::{ActionRuleScope, TenantId};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::procedure::error::ProcedureError;
use moa_skills::procedure::runtime::{ProcedureRuntime, StartProcedureRun};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry, StoredSkillPackage};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::authorize_tenant;
use crate::workflows::errors::{moa_error_to_status_handler_error, procedure_handler_error};
use crate::workflows::procedure_execution::{
    ProcedureExecutionClient, RunProcedureRequest, validate_procedure_review_decision,
    validate_procedure_signal,
};

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

    /// Creates a durable procedure run for a published skill that carries a procedure.
    async fn run(
        request: Json<ProcedureRunRequest>,
    ) -> Result<Json<ProcedureRunResponse>, HandlerError>;

    /// Loads procedure run status.
    async fn status(
        request: Json<ProcedureStatusRequest>,
    ) -> Result<Json<ProcedureRunStatus>, HandlerError>;

    /// Requests procedure run cancellation.
    async fn cancel(
        request: Json<ProcedureCancelRequest>,
    ) -> Result<Json<ProcedureCancelResponse>, HandlerError>;

    /// Decides a pending procedure review node.
    async fn decide_review(
        request: Json<ProcedureReviewDecisionRequest>,
    ) -> Result<Json<ProcedureReviewDecisionResponse>, HandlerError>;

    /// Resolves a pending procedure wait-signal node.
    async fn signal(
        request: Json<ProcedureSignalRequest>,
    ) -> Result<Json<ProcedureSignalResponse>, HandlerError>;
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

    #[tracing::instrument(skip(self, ctx, request))]
    async fn run(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureRunRequest>,
    ) -> Result<Json<ProcedureRunResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "run");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let execution_request_tenant_id = request.tenant_id;
        let execution_request_session_id = request.session_id;

        let response = ctx
            .run(|| async move { run_inner(request).await.map(Json::from) })
            .name("skills_run")
            .await?
            .into_inner();
        ctx.workflow_client::<ProcedureExecutionClient>(response.run_id.to_string())
            .run(Json::from(RunProcedureRequest {
                tenant_id: execution_request_tenant_id,
                run_uid: response.run_id,
                identity,
                session_id: execution_request_session_id,
            }))
            .send();
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn status(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureStatusRequest>,
    ) -> Result<Json<ProcedureRunStatus>, HandlerError> {
        annotate_restate_handler_span("Skills", "status");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        Ok(ctx
            .run(|| async move { status_inner(request).await.map(Json::from) })
            .name("skills_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn cancel(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureCancelRequest>,
    ) -> Result<Json<ProcedureCancelResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "cancel");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let run_uid = request.run_id;
        let reason = request
            .reason
            .clone()
            .unwrap_or_else(|| "procedure cancellation requested".to_string());

        let response = ctx
            .run(|| async move { cancel_inner(request).await.map(Json::from) })
            .name("skills_cancel")
            .await?
            .into_inner();
        if response.cancelled {
            ctx.workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
                .request_cancel(Json::from(reason))
                .send();
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide_review(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureReviewDecisionRequest>,
    ) -> Result<Json<ProcedureReviewDecisionResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "decide_review");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let run_uid = request.run_id;

        let validated = ctx
            .run(|| async move {
                validate_procedure_review_decision(request)
                    .await
                    .map(Json::from)
            })
            .name("skills_decide_review")
            .await?
            .into_inner();
        if let Some(resolution) = validated.resolution {
            return ctx
                .workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
                .decide_review(Json::from(resolution))
                .call()
                .await
                .map_err(HandlerError::from);
        }
        Ok(Json::from(validated.response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn signal(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureSignalRequest>,
    ) -> Result<Json<ProcedureSignalResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "signal");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;
        let run_uid = request.run_id;

        let validated = ctx
            .run(|| async move { validate_procedure_signal(request).await.map(Json::from) })
            .name("skills_signal")
            .await?
            .into_inner();
        if let Some(resolution) = validated.resolution {
            return ctx
                .workflow_client::<ProcedureExecutionClient>(run_uid.to_string())
                .signal(Json::from(resolution))
                .call()
                .await
                .map_err(HandlerError::from);
        }
        Ok(Json::from(validated.response))
    }
}

async fn run_inner(request: ProcedureRunRequest) -> Result<ProcedureRunResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime()
        .start(
            &scope,
            StartProcedureRun {
                procedure_ref: request.procedure_ref,
                input: request.input,
                session_id: request.session_id,
                idempotency_key: request.idempotency_key,
            },
        )
        .await
        .map_err(procedure_handler_error)?;

    Ok(ProcedureRunResponse {
        run_id: run.run_uid,
        status: run.status.as_str().to_string(),
    })
}

async fn status_inner(request: ProcedureStatusRequest) -> Result<ProcedureRunStatus, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime()
        .status(&scope, request.run_id)
        .await
        .map_err(procedure_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    let node_runs = ArtifactRegistry::new(OrchestratorCtx::current_graph_pool())
        .list_node_runs(&scope, request.run_id)
        .await
        .map_err(|error| procedure_handler_error(ProcedureError::Artifact(error)))?
        .into_iter()
        .map(|node_run| ProcedureNodeRunSummary {
            node_id: node_run.node_id,
            status: node_run.status.as_str().to_string(),
            started_at: node_run.started_at,
            completed_at: node_run.completed_at,
        })
        .collect();
    Ok(ProcedureRunStatus {
        run_id: run.run_uid,
        session_id: run.session_id,
        current_node_id: run.current_node_id,
        status: run.status.as_str().to_string(),
        node_runs,
        output: run.output,
        error: run.error,
    })
}

async fn cancel_inner(
    request: ProcedureCancelRequest,
) -> Result<ProcedureCancelResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime()
        .cancel(&scope, request.run_id, request.reason)
        .await
        .map_err(procedure_handler_error)?;
    Ok(ProcedureCancelResponse {
        cancelled: run.is_some(),
        reason: run
            .map(|_| "cancelled".to_string())
            .unwrap_or_else(|| "procedure run was not cancellable".to_string()),
    })
}

fn procedure_runtime() -> ProcedureRuntime {
    ProcedureRuntime::new(ArtifactRegistry::new(OrchestratorCtx::current_graph_pool()))
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
            .map_err(moa_error_to_status_handler_error)?;
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
        .map_err(moa_error_to_status_handler_error)?;
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
