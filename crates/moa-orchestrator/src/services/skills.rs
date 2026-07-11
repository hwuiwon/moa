//! Restate service for cloud-owned skill import, export, listing, and
//! skill-backed procedure run lifecycle operations.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::action::ActionDefinition;
use moa_artifacts::connector::ConnectorDefinition;
use moa_artifacts::document::{ArtifactDefinition, ArtifactKind, ArtifactStatus};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::registry::{ArtifactRegistry, ArtifactRunListRequest, StoredArtifactRevision};
use moa_artifacts::skill::SkillActionDefinition;
use moa_authz_schema::Relation;
use moa_core::wire::capabilities::{
    CapabilitiesListRequest, CapabilitiesListResponse, CapabilityEntry, CapabilityKind,
};
use moa_core::wire::knowledge::KnowledgeConnectionSummary;
use moa_core::wire::procedures::{
    ProcedureCancelRequest, ProcedureCancelResponse, ProcedureNodeRunSummary,
    ProcedureReviewDecisionRequest, ProcedureReviewDecisionResponse, ProcedureRunListCursor,
    ProcedureRunListRequest, ProcedureRunListResponse, ProcedureRunRequest, ProcedureRunResponse,
    ProcedureRunStatus, ProcedureRunSummary, ProcedureSignalRequest, ProcedureSignalResponse,
    ProcedureStatusRequest,
};
use moa_core::wire::skills::{
    SkillExportRequest, SkillExportResponse, SkillImportRequest, SkillImportResponse,
    SkillListRequest, SkillListResponse, SkillPackageDocument, SkillPackageDocumentFile,
    SkillSummary,
};
use moa_core::{
    types::action_policy::ActionRuleScope, types::identifiers::TenantId, types::memory::RlsContext,
    types::procedure_tools::procedure_tool_schemas,
    types::worker::tool_schema::delegation_tool_schemas,
};
use moa_hands::ToolRegistry;
use moa_knowledge::repository::{KnowledgeRepository, PostgresKnowledgeRepository};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_skills::package::{SkillPackage, SkillPackageFile};
use moa_skills::procedure::error::ProcedureError;
use moa_skills::procedure::runtime::{ProcedureRuntime, StartProcedureRun};
use moa_skills::registry::{NewSkill, Skill, SkillRegistry, StoredSkillPackage};
use restate_sdk::prelude::*;
use serde_json::Value;

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

    /// Lists the tenant capabilities a procedure step can attach to.
    ///
    /// Merges built-in tools, published action/connector and skill artifacts,
    /// graph-memory operations, and knowledge datasources into one deterministic
    /// catalog after a tenant operator check. Tool coverage is limited to the
    /// statically declarable set; see `list_capabilities_inner` for the exact
    /// limitation.
    async fn list_capabilities(
        request: Json<CapabilitiesListRequest>,
    ) -> Result<Json<CapabilitiesListResponse>, HandlerError>;

    /// Creates a durable procedure run for a published skill that carries a procedure.
    async fn run(
        request: Json<ProcedureRunRequest>,
    ) -> Result<Json<ProcedureRunResponse>, HandlerError>;

    /// Loads procedure run status.
    async fn status(
        request: Json<ProcedureStatusRequest>,
    ) -> Result<Json<ProcedureRunStatus>, HandlerError>;

    /// Lists procedure runs after a tenant operator check.
    async fn list_runs(
        request: Json<ProcedureRunListRequest>,
    ) -> Result<Json<ProcedureRunListResponse>, HandlerError>;

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
#[derive(Clone)]
pub struct SkillsImpl {
    pool: sqlx::PgPool,
}

impl SkillsImpl {
    /// Creates the skills adapter with its artifact, skill, and knowledge pool.
    #[must_use]
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }
}

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

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { export_inner(pool, request).await.map(Json::from) })
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

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { import_inner(pool, scope, packages).await.map(Json::from) })
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

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_inner(pool, request).await.map(Json::from) })
            .name("skills_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_capabilities(
        &self,
        ctx: Context<'_>,
        request: Json<CapabilitiesListRequest>,
    ) -> Result<Json<CapabilitiesListResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "list_capabilities");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_capabilities_inner(pool, request).await.map(Json::from) })
            .name("skills_list_capabilities")
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

        let pool = self.pool.clone();
        let response = ctx
            .run(|| async move { run_inner(pool, request).await.map(Json::from) })
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

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { status_inner(pool, request).await.map(Json::from) })
            .name("skills_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_runs(
        &self,
        ctx: Context<'_>,
        request: Json<ProcedureRunListRequest>,
    ) -> Result<Json<ProcedureRunListResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "list_runs");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Operator).await?;

        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move { list_runs_inner(pool, request).await.map(Json::from) })
            .name("skills_list_runs")
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

        let pool = self.pool.clone();
        let response = ctx
            .run(|| async move { cancel_inner(pool, request).await.map(Json::from) })
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
        let registry = ArtifactRegistry::new(self.pool.clone());

        let validated = ctx
            .run(|| async move {
                validate_procedure_review_decision(registry, request)
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
        let registry = ArtifactRegistry::new(self.pool.clone());

        let validated = ctx
            .run(|| async move {
                validate_procedure_signal(registry, request)
                    .await
                    .map(Json::from)
            })
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

async fn run_inner(
    pool: sqlx::PgPool,
    request: ProcedureRunRequest,
) -> Result<ProcedureRunResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime(pool)
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

async fn status_inner(
    pool: sqlx::PgPool,
    request: ProcedureStatusRequest,
) -> Result<ProcedureRunStatus, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime(pool.clone())
        .status(&scope, request.run_id)
        .await
        .map_err(procedure_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "procedure run not found"))?;
    let node_runs = ArtifactRegistry::new(pool)
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

async fn list_runs_inner(
    pool: sqlx::PgPool,
    request: ProcedureRunListRequest,
) -> Result<ProcedureRunListResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let status = request
        .status
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|error: moa_core::error::MoaError| {
            TerminalError::new_with_code(400, error.to_string())
        })?;
    let page = ArtifactRegistry::new(pool)
        .list_runs(
            &scope,
            ArtifactRunListRequest {
                status,
                limit: request.limit,
                cursor: request.cursor.map(|cursor| {
                    moa_artifacts::registry::ArtifactRunListCursor {
                        started_at: cursor.started_at,
                        run_uid: cursor.run_id,
                    }
                }),
            },
        )
        .await
        .map_err(|error| procedure_handler_error(ProcedureError::Artifact(error)))?;
    Ok(ProcedureRunListResponse {
        tenant_id: request.tenant_id,
        runs: page
            .runs
            .into_iter()
            .map(procedure_run_summary_from_run)
            .collect(),
        next_cursor: page.next_cursor.map(|cursor| ProcedureRunListCursor {
            started_at: cursor.started_at,
            run_id: cursor.run_uid,
        }),
    })
}

/// Converts a registry procedure run into a public list summary.
pub fn procedure_run_summary_from_run(
    run: moa_artifacts::registry::ArtifactRun,
) -> ProcedureRunSummary {
    ProcedureRunSummary {
        run_id: run.run_uid,
        artifact_uid: run.artifact_uid,
        revision_uid: run.revision_uid,
        session_id: run.session_id,
        procedure_ref: run.procedure_ref,
        status: run.status.as_str().to_string(),
        current_node_id: run.current_node_id,
        started_at: run.started_at,
        completed_at: run.completed_at,
    }
}

async fn cancel_inner(
    pool: sqlx::PgPool,
    request: ProcedureCancelRequest,
) -> Result<ProcedureCancelResponse, HandlerError> {
    let scope = ActionRuleScope::Tenant {
        tenant_id: request.tenant_id,
    };
    let run = procedure_runtime(pool)
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

fn procedure_runtime(pool: sqlx::PgPool) -> ProcedureRuntime {
    ProcedureRuntime::new(ArtifactRegistry::new(pool))
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

async fn import_inner(
    pool: sqlx::PgPool,
    scope: ActionRuleScope,
    packages: Vec<SkillPackageDocument>,
) -> Result<SkillImportResponse, HandlerError> {
    let registry = skill_registry(pool);
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

/// Builds the tenant capabilities catalog from every attachable source.
///
/// Merges statically declared built-in tools, published action/connector and
/// skill artifacts, the two graph-memory operations, and tenant knowledge
/// datasources into one deterministic, sorted list. All source loading happens
/// here so the ordering and reference-construction logic can live in the pure,
/// unit-testable [`build_capabilities`].
///
/// Tool coverage is limited to the statically declarable set: the default local
/// hand/built-in tool registry plus the built-in delegation and procedure tools.
/// Per-turn or configuration-dependent tools — MCP-discovered tools and the
/// child-only report tools exposed only inside a worker subset — cannot be
/// enumerated from a tenant-scoped read and are intentionally omitted.
async fn list_capabilities_inner(
    pool: sqlx::PgPool,
    request: CapabilitiesListRequest,
) -> Result<CapabilitiesListResponse, HandlerError> {
    let tenant_id = request.tenant_id;
    let scope = ActionRuleScope::Tenant { tenant_id };
    let registry = ArtifactRegistry::new(pool.clone());

    let tool_sources = builtin_tool_sources();
    let action_artifacts = load_action_artifacts(&registry, &scope).await?;
    let connector_artifacts = load_connector_artifacts(&registry, &scope).await?;
    let skill_actions = load_skill_actions(&registry, &scope).await?;
    let datasources = load_datasource_summaries(pool, tenant_id).await?;

    let capabilities = build_capabilities(
        &tool_sources,
        &action_artifacts,
        &connector_artifacts,
        &skill_actions,
        &datasources,
    );
    Ok(CapabilitiesListResponse { capabilities })
}

/// One statically declared built-in tool contributing a `Tool` capability entry.
struct ToolCapabilitySource {
    /// Provider-facing tool name, reused as the stable attachment reference.
    name: String,
    /// Tool description surfaced to the builder.
    description: String,
    /// Tool input JSON schema.
    input_schema: Value,
}

impl ToolCapabilitySource {
    /// Extracts a tool source from an Anthropic-shaped tool schema, if well-formed.
    fn from_anthropic_schema(schema: &Value) -> Option<Self> {
        Some(Self {
            name: schema.get("name")?.as_str()?.to_string(),
            description: schema
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input_schema: schema
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        })
    }
}

/// Collects the statically declarable built-in tool schemas as capability sources.
///
/// Combines the default local hand/built-in tool registry with the built-in
/// delegation and procedure tool schemas; all three expose Anthropic-shaped
/// `{name, description, input_schema}` payloads.
fn builtin_tool_sources() -> Vec<ToolCapabilitySource> {
    ToolRegistry::default_local()
        .default_tool_schemas()
        .into_iter()
        .chain(delegation_tool_schemas())
        .chain(procedure_tool_schemas())
        .filter_map(|schema| ToolCapabilitySource::from_anthropic_schema(&schema))
        .collect()
}

/// Loads published standalone action artifacts as `(artifact_name, definition)` pairs.
async fn load_action_artifacts(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> Result<Vec<(String, ActionDefinition)>, HandlerError> {
    let mut sources = Vec::new();
    for revision in load_published_revisions(registry, scope, ArtifactKind::Action).await? {
        let StoredArtifactRevision { name, document, .. } = revision;
        if let ArtifactDefinition::Action(definition) = document.definition {
            sources.push((name, definition));
        }
    }
    Ok(sources)
}

/// Loads published connector artifacts as `(connector_name, definition)` pairs.
async fn load_connector_artifacts(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> Result<Vec<(String, ConnectorDefinition)>, HandlerError> {
    let mut sources = Vec::new();
    for revision in load_published_revisions(registry, scope, ArtifactKind::Connector).await? {
        let StoredArtifactRevision { name, document, .. } = revision;
        if let ArtifactDefinition::Connector(definition) = document.definition {
            sources.push((name, definition));
        }
    }
    Ok(sources)
}

/// Loads published skill artifacts as `(skill_name, actions)` pairs, ignoring procedures.
async fn load_skill_actions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
) -> Result<Vec<(String, Vec<SkillActionDefinition>)>, HandlerError> {
    let mut sources = Vec::new();
    for revision in load_published_revisions(registry, scope, ArtifactKind::Skill).await? {
        let StoredArtifactRevision { name, document, .. } = revision;
        if let ArtifactDefinition::Skill(definition) = document.definition {
            sources.push((name, definition.actions));
        }
    }
    Ok(sources)
}

/// Loads every published revision document for one artifact kind in a scope.
///
/// The list query returns summaries without the definition body, so each visible
/// revision is loaded to read its callable actions and input schemas. This N+1
/// read is acceptable for a low-frequency, bounded tenant-admin catalog request.
async fn load_published_revisions(
    registry: &ArtifactRegistry,
    scope: &ActionRuleScope,
    kind: ArtifactKind,
) -> Result<Vec<StoredArtifactRevision>, HandlerError> {
    let summaries = registry
        .list_visible(scope, Some(kind), Some(ArtifactStatus::Published))
        .await
        .map_err(|error| procedure_handler_error(ProcedureError::Artifact(error)))?;
    let mut revisions = Vec::with_capacity(summaries.len());
    for summary in summaries {
        if let Some(revision) = registry
            .load_revision(scope, summary.revision_uid)
            .await
            .map_err(|error| procedure_handler_error(ProcedureError::Artifact(error)))?
        {
            revisions.push(revision);
        }
    }
    Ok(revisions)
}

/// Lists tenant knowledge connections as datasource summaries, read-only.
///
/// Reuses the same tenant-scoped repository read the `Knowledge` service uses for
/// `list_connections`, without touching that service's internals.
async fn load_datasource_summaries(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
) -> Result<Vec<KnowledgeConnectionSummary>, HandlerError> {
    let repository = PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id));
    let projections = repository
        .list_connections(tenant_id, None)
        .await
        .map_err(datasource_handler_error)?;
    Ok(projections
        .into_iter()
        .map(|projection| KnowledgeConnectionSummary {
            connection_uid: projection.connection.connection_uid,
            provider: projection.connection.provider,
            connector: projection.connection.connector,
            provider_account_id: projection.connection.provider_account_id,
            status: projection.connection.status.as_str().to_string(),
            last_sync_status: projection
                .last_sync_status
                .map(|status| status.as_str().to_string()),
            last_synced_at: projection.connection.last_synced_at,
            source_selection: projection.connection.source_selection,
            credential_status: None,
        })
        .collect())
}

/// Maps a knowledge datasource read failure into a terminal handler error.
fn datasource_handler_error(error: moa_knowledge::Error) -> HandlerError {
    tracing::error!(error = %error, "capabilities datasource listing failed");
    TerminalError::new_with_code(500, "failed to list knowledge datasources").into()
}

/// Merges pre-fetched capability sources into one deterministic, sorted catalog.
///
/// Pure and side-effect free so the ordering and reference construction can be
/// unit-tested without a database or Restate context. Output is sorted by kind,
/// then display name, then reference so equal names stay stably ordered.
fn build_capabilities(
    tools: &[ToolCapabilitySource],
    action_artifacts: &[(String, ActionDefinition)],
    connector_artifacts: &[(String, ConnectorDefinition)],
    skill_actions: &[(String, Vec<SkillActionDefinition>)],
    datasources: &[KnowledgeConnectionSummary],
) -> Vec<CapabilityEntry> {
    let mut entries = Vec::new();

    for tool in tools {
        entries.push(CapabilityEntry {
            kind: CapabilityKind::Tool,
            name: tool.name.clone(),
            reference: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: Some(tool.input_schema.clone()),
            source: "builtin".to_string(),
        });
    }

    for (artifact_name, action) in action_artifacts {
        entries.push(CapabilityEntry {
            kind: CapabilityKind::ConnectorAction,
            name: artifact_name.clone(),
            reference: ArtifactRef::action_artifact(artifact_name.clone()).to_string(),
            description: action.description.clone(),
            input_schema: Some(action.input_schema.clone()),
            source: "artifact".to_string(),
        });
    }

    for (connector_name, connector) in connector_artifacts {
        for action in &connector.actions {
            entries.push(CapabilityEntry {
                kind: CapabilityKind::ConnectorAction,
                name: format!("{connector_name}.{}", action.id),
                reference: ArtifactRef::action(connector_name.clone(), action.id.clone())
                    .to_string(),
                description: action.description.clone(),
                input_schema: Some(action.input_schema.clone()),
                source: "artifact".to_string(),
            });
        }
    }

    for (skill_name, actions) in skill_actions {
        for action in actions {
            entries.push(CapabilityEntry {
                kind: CapabilityKind::SkillAction,
                name: format!("{skill_name}#{}", action.id),
                reference: format!("skill://{skill_name}#{}", action.id),
                description: action.description.clone(),
                input_schema: Some(action.input_schema.clone()),
                source: "artifact".to_string(),
            });
        }
    }

    entries.extend(memory_capability_entries());

    for datasource in datasources {
        entries.push(CapabilityEntry {
            kind: CapabilityKind::Datasource,
            name: datasource.connector.clone(),
            reference: datasource.connection_uid.to_string(),
            description: format!(
                "{} datasource '{}' (status: {})",
                datasource.provider, datasource.connector, datasource.status
            ),
            input_schema: None,
            source: format!("knowledge_connection:{}", datasource.provider),
        });
    }

    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.reference.cmp(&right.reference))
    });
    entries
}

/// Returns the two static graph-memory capability entries.
///
/// Descriptions mirror the `MemoryRead`/`MemoryWrite` procedure nodes: a read
/// retrieves graph-memory facts for the scope matching a query, and a write
/// persists facts or documents into graph memory for the scope.
fn memory_capability_entries() -> [CapabilityEntry; 2] {
    [
        CapabilityEntry {
            kind: CapabilityKind::Memory,
            name: "Memory read".to_string(),
            reference: "memory_read".to_string(),
            description: "Retrieve facts from graph memory for the tenant and contact scope that match a query.".to_string(),
            input_schema: None,
            source: "builtin".to_string(),
        },
        CapabilityEntry {
            kind: CapabilityKind::Memory,
            name: "Memory write".to_string(),
            reference: "memory_write".to_string(),
            description: "Persist a fact or documents into graph memory for the tenant and contact scope.".to_string(),
            input_schema: None,
            source: "builtin".to_string(),
        },
    ]
}

fn skill_registry(pool: sqlx::PgPool) -> SkillRegistry {
    SkillRegistry::new(pool)
}

async fn authorized_import_scope(
    ctx: &impl RequestHeaders,
    scope: ActionRuleScope,
) -> Result<ActionRuleScope, HandlerError> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
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

#[cfg(test)]
mod capabilities_tests {
    use moa_artifacts::skill::SkillActionKind;
    use uuid::Uuid;

    use super::*;

    fn action_definition(id: &str, description: &str, input_schema: Value) -> ActionDefinition {
        ActionDefinition {
            id: id.to_string(),
            description: description.to_string(),
            connector_ref: None,
            tool_name: None,
            input_schema,
            output_schema: serde_json::json!({}),
            admin_review_required: false,
            ui: serde_json::json!({}),
        }
    }

    fn connector_action(
        id: &str,
        input_schema: Value,
    ) -> moa_artifacts::connector::ConnectorActionDefinition {
        moa_artifacts::connector::ConnectorActionDefinition {
            id: id.to_string(),
            description: format!("{id} action"),
            tool_name: None,
            input_schema,
            output_schema: serde_json::json!({}),
            admin_review_required: false,
            ui: serde_json::json!({}),
        }
    }

    fn skill_action(id: &str, input_schema: Value) -> SkillActionDefinition {
        SkillActionDefinition {
            id: id.to_string(),
            description: format!("{id} skill action"),
            kind: SkillActionKind::Tool,
            artifact_ref: None,
            runtime: None,
            entrypoint: None,
            input_schema,
            output_schema: serde_json::json!({}),
            ui: serde_json::json!({}),
        }
    }

    fn datasource(
        uid: u128,
        connector: &str,
        provider: &str,
        status: &str,
    ) -> KnowledgeConnectionSummary {
        KnowledgeConnectionSummary {
            connection_uid: Uuid::from_u128(uid),
            provider: provider.to_string(),
            connector: connector.to_string(),
            provider_account_id: "acct-1".to_string(),
            status: status.to_string(),
            last_sync_status: None,
            last_synced_at: None,
            source_selection: serde_json::json!({}),
            credential_status: None,
        }
    }

    fn entry<'a>(entries: &'a [CapabilityEntry], reference: &str) -> &'a CapabilityEntry {
        entries
            .iter()
            .find(|entry| entry.reference == reference)
            .unwrap_or_else(|| panic!("expected an entry with reference {reference}"))
    }

    #[test]
    fn build_capabilities_assigns_stable_references_per_kind() {
        // Pins: each source kind maps to the stable attachment reference a ProcedureNode
        // uses — bare tool name, action://<name>, action://<connector>.<action>,
        // skill://<skill>#<action>, memory_read/memory_write, and the connection id.
        let tools = vec![ToolCapabilitySource {
            name: "bash".to_string(),
            description: "run a command".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        }];
        let action_artifacts = vec![(
            "refund".to_string(),
            action_definition(
                "refund",
                "issue a refund",
                serde_json::json!({"type": "object"}),
            ),
        )];
        let connector_artifacts = vec![(
            "stripe".to_string(),
            ConnectorDefinition {
                auth: serde_json::json!({}),
                actions: vec![connector_action(
                    "charge",
                    serde_json::json!({"type": "object"}),
                )],
                ui: serde_json::json!({}),
            },
        )];
        let skill_actions = vec![(
            "onboarding".to_string(),
            vec![skill_action(
                "welcome",
                serde_json::json!({"type": "object"}),
            )],
        )];
        // Two linked-account providers prove datasource entries stay provider-agnostic:
        // the store's provider id is carried in `source`, never special-cased.
        let datasources = vec![
            datasource(0x1234, "google-drive", "nango", "active"),
            datasource(0x5678, "salesforce", "merge", "syncing"),
        ];

        let entries = build_capabilities(
            &tools,
            &action_artifacts,
            &connector_artifacts,
            &skill_actions,
            &datasources,
        );

        let bash = entry(&entries, "bash");
        assert_eq!(bash.kind, CapabilityKind::Tool);
        assert_eq!(bash.source, "builtin");
        assert!(bash.input_schema.is_some());

        let refund = entry(&entries, "action://refund");
        assert_eq!(refund.kind, CapabilityKind::ConnectorAction);
        assert_eq!(refund.name, "refund");
        assert_eq!(refund.source, "artifact");

        let charge = entry(&entries, "action://stripe.charge");
        assert_eq!(charge.kind, CapabilityKind::ConnectorAction);
        assert_eq!(charge.name, "stripe.charge");

        let welcome = entry(&entries, "skill://onboarding#welcome");
        assert_eq!(welcome.kind, CapabilityKind::SkillAction);
        assert_eq!(welcome.name, "onboarding#welcome");

        let read = entry(&entries, "memory_read");
        assert_eq!(read.kind, CapabilityKind::Memory);
        assert!(read.input_schema.is_none());
        assert!(
            entries
                .iter()
                .any(|entry| entry.reference == "memory_write")
        );

        let nango = entry(&entries, &Uuid::from_u128(0x1234).to_string());
        assert_eq!(nango.kind, CapabilityKind::Datasource);
        assert_eq!(nango.name, "google-drive");
        assert_eq!(nango.source, "knowledge_connection:nango");
        assert!(nango.input_schema.is_none());
        assert!(nango.description.contains("nango"));

        let merge = entry(&entries, &Uuid::from_u128(0x5678).to_string());
        assert_eq!(merge.kind, CapabilityKind::Datasource);
        assert_eq!(merge.name, "salesforce");
        assert_eq!(merge.source, "knowledge_connection:merge");
    }

    #[test]
    fn build_capabilities_sorts_by_kind_then_name() {
        // Pins: output is deterministic — grouped by kind in declaration order, then by name,
        // so the dashboard dropdown renders the same list for the same tenant state.
        let tools = vec![
            ToolCapabilitySource {
                name: "grep".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
            ToolCapabilitySource {
                name: "bash".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        ];

        let entries = build_capabilities(&tools, &[], &[], &[], &[]);

        let kinds: Vec<CapabilityKind> = entries.iter().map(|entry| entry.kind).collect();
        let mut sorted_kinds = kinds.clone();
        sorted_kinds.sort();
        assert_eq!(kinds, sorted_kinds, "entries must be grouped by kind order");

        let tool_names: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.kind == CapabilityKind::Tool)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(
            tool_names,
            vec!["bash", "grep"],
            "tools must be name-sorted"
        );

        // Memory entries are always present even with no artifacts or datasources.
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.kind == CapabilityKind::Memory)
                .count(),
            2
        );
    }

    #[test]
    fn builtin_tool_sources_enumerate_static_registry_and_agent_tools() {
        // Pins: the statically declarable tool set is enumerable offline and includes the
        // hand registry, delegation, and procedure tools that a procedure step can attach to.
        let names: Vec<String> = builtin_tool_sources()
            .into_iter()
            .map(|source| source.name)
            .collect();
        assert!(
            names.iter().any(|name| name == "bash"),
            "hand tools present"
        );
        assert!(
            names.iter().any(|name| name == "spawn_worker"),
            "delegation tools present"
        );
        assert!(
            names.iter().any(|name| name == "run_procedure"),
            "procedure tools present"
        );
    }
}
