//! Restate service for canonical artifact import, export, validation, and publish requests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, StoredArtifactRevision,
};
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::validate_for_status;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    ArtifactExportRequest, ArtifactExportResponse, ArtifactFileDocument, ArtifactImportRequest,
    ArtifactImportResponse, ArtifactListRequest, ArtifactListResponse, ArtifactPublishRequest,
    ArtifactPublishResponse, ArtifactSummary, ArtifactValidateRequest, ArtifactValidateResponse,
};
use moa_core::{MemoryScope, MoaError, WorkspaceId};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::skills::{SkillScopeError, checked_import_scope};

/// Restate service surface for protected artifact operations.
#[restate_sdk::service]
#[name = "Artifacts"]
pub trait Artifacts {
    /// Imports a draft artifact revision.
    async fn import(
        request: Json<ArtifactImportRequest>,
    ) -> Result<Json<ArtifactImportResponse>, HandlerError>;

    /// Exports a visible artifact revision.
    async fn export(
        request: Json<ArtifactExportRequest>,
    ) -> Result<Json<ArtifactExportResponse>, HandlerError>;

    /// Lists visible artifact revisions.
    async fn list(
        request: Json<ArtifactListRequest>,
    ) -> Result<Json<ArtifactListResponse>, HandlerError>;

    /// Validates an artifact document without writing it.
    async fn validate(
        request: Json<ArtifactValidateRequest>,
    ) -> Result<Json<ArtifactValidateResponse>, HandlerError>;

    /// Publishes a draft artifact revision.
    async fn publish(
        request: Json<ArtifactPublishRequest>,
    ) -> Result<Json<ArtifactPublishResponse>, HandlerError>;
}

/// Concrete artifact service implementation.
#[derive(Clone, Default)]
pub struct ArtifactsImpl;

impl Artifacts for ArtifactsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn import(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactImportRequest>,
    ) -> Result<Json<ArtifactImportResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "import");
        let request = request.into_inner();
        reject_scope_workspace_mismatch(&request.workspace_id, &request.scope)?;
        let scope =
            authorized_write_scope(&ctx, &request.workspace_id, request.scope.clone()).await?;

        Ok(ctx
            .run(|| async move { import_inner(scope, request).await.map(Json::from) })
            .name("artifacts_import")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn export(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactExportRequest>,
    ) -> Result<Json<ArtifactExportResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "export");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { export_inner(request).await.map(Json::from) })
            .name("artifacts_export")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactListRequest>,
    ) -> Result<Json<ArtifactListResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "list");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { list_inner(request).await.map(Json::from) })
            .name("artifacts_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn validate(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactValidateRequest>,
    ) -> Result<Json<ArtifactValidateResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "validate");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { validate_inner(request).map(Json::from) })
            .name("artifacts_validate")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn publish(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactPublishRequest>,
    ) -> Result<Json<ArtifactPublishResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "publish");
        let request = request.into_inner();
        reject_scope_workspace_mismatch(&request.workspace_id, &request.scope)?;
        let scope =
            authorized_write_scope(&ctx, &request.workspace_id, request.scope.clone()).await?;

        Ok(ctx
            .run(|| async move { publish_inner(scope, request).await.map(Json::from) })
            .name("artifacts_publish")
            .await?)
    }
}

async fn import_inner(
    scope: MemoryScope,
    request: ArtifactImportRequest,
) -> Result<ArtifactImportResponse, HandlerError> {
    let document = parse_document(&request.source_format, &request.source_text)?;
    let files = decode_files(request.files)?;
    let stored = artifact_registry()
        .create_draft(
            &scope,
            NewArtifactDraft {
                document: &document,
                source_format: &request.source_format,
                source_text: request.source_text.as_bytes(),
                files: &files,
            },
        )
        .await
        .map_err(artifact_handler_error)?;

    Ok(ArtifactImportResponse {
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        status: stored.status.to_string(),
        validation_report: stored.validation_report,
    })
}

async fn export_inner(
    request: ArtifactExportRequest,
) -> Result<ArtifactExportResponse, HandlerError> {
    let scope = request.scope.unwrap_or(MemoryScope::Workspace {
        workspace_id: request.workspace_id,
    });
    let kind = parse_kind(&request.kind)?;
    let registry = artifact_registry();
    let stored = registry
        .load_visible(&scope, kind, &request.name)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "artifact not found"))?;
    let files = registry
        .load_files(&scope, stored.revision_uid)
        .await
        .map_err(artifact_handler_error)?
        .into_iter()
        .map(|file| ArtifactFileDocument {
            path: file.path,
            content_base64: BASE64.encode(file.content),
            content_type: file.content_type,
            executable: file.executable,
        })
        .collect();
    let document = serde_json::to_value(&stored.document)
        .map_err(|error| TerminalError::new_with_code(500, error.to_string()))?;
    let source_text = String::from_utf8(stored.source_text)
        .map_err(|error| TerminalError::new_with_code(500, error.to_string()))?;

    Ok(ArtifactExportResponse {
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        source_format: stored.source_format,
        source_text,
        document,
        files,
    })
}

async fn list_inner(request: ArtifactListRequest) -> Result<ArtifactListResponse, HandlerError> {
    let scope = request.scope.unwrap_or(MemoryScope::Workspace {
        workspace_id: request.workspace_id,
    });
    let kind = request.kind.as_deref().map(parse_kind).transpose()?;
    let status = request.status.as_deref().map(parse_status).transpose()?;
    let artifacts = artifact_registry()
        .list_visible(&scope, kind, status)
        .await
        .map_err(artifact_handler_error)?
        .into_iter()
        .map(|summary| ArtifactSummary {
            artifact_uid: summary.artifact_uid,
            revision_uid: summary.revision_uid,
            scope: summary.scope,
            kind: summary.kind.to_string(),
            name: summary.name,
            description: summary.description,
            tags: summary.tags,
            status: summary.status.to_string(),
            version: summary.version,
            updated_at: summary.updated_at,
        })
        .collect();
    Ok(ArtifactListResponse { artifacts })
}

fn validate_inner(
    request: ArtifactValidateRequest,
) -> Result<ArtifactValidateResponse, HandlerError> {
    let document = parse_document(&request.source_format, &request.source_text)?;
    let status = request
        .status
        .as_deref()
        .map(parse_status)
        .transpose()?
        .unwrap_or(ArtifactStatus::Draft);
    let report = validate_for_status(&document, status);
    let valid = report.is_ok();
    let validation_report = serde_json::to_value(report)
        .map_err(|error| TerminalError::new_with_code(500, error.to_string()))?;
    Ok(ArtifactValidateResponse {
        valid,
        validation_report,
    })
}

async fn publish_inner(
    scope: MemoryScope,
    request: ArtifactPublishRequest,
) -> Result<ArtifactPublishResponse, HandlerError> {
    let registry = artifact_registry();
    let stored = registry
        .load_revision(&scope, request.revision_uid)
        .await
        .map_err(artifact_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "artifact revision not found"))?;
    let mut document = stored.document.clone();
    document.reference_resolutions = ArtifactResolver::new(artifact_registry())
        .resolve_document(&scope, &document)
        .await
        .map_err(artifact_handler_error)?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        return Err(
            TerminalError::new_with_code(400, "artifact revision is not publishable").into(),
        );
    }

    let published = registry
        .publish_revision(&scope, request.revision_uid, &report)
        .await
        .map_err(artifact_handler_error)?;
    let validation_report = serde_json::to_value(report)
        .map_err(|error| TerminalError::new_with_code(500, error.to_string()))?;
    Ok(publish_response(published, validation_report))
}

fn publish_response(
    stored: StoredArtifactRevision,
    validation_report: serde_json::Value,
) -> ArtifactPublishResponse {
    ArtifactPublishResponse {
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        status: stored.status.to_string(),
        validation_report,
    }
}

fn parse_document(
    source_format: &str,
    source_text: &str,
) -> Result<ArtifactDocument, HandlerError> {
    match source_format {
        "json" => ArtifactDocument::from_json(source_text),
        "yaml" => ArtifactDocument::from_yaml(source_text),
        other => {
            return Err(TerminalError::new_with_code(
                400,
                format!("unsupported artifact source format `{other}`"),
            )
            .into());
        }
    }
    .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

fn decode_files(files: Vec<ArtifactFileDocument>) -> Result<Vec<NewArtifactFile>, HandlerError> {
    files
        .into_iter()
        .map(|file| {
            let content = BASE64.decode(&file.content_base64).map_err(|error| {
                HandlerError::from(TerminalError::new_with_code(
                    400,
                    format!(
                        "artifact file `{}` content_base64 is invalid: {error}",
                        file.path
                    ),
                ))
            })?;
            Ok(NewArtifactFile {
                path: file.path,
                content,
                content_type: file.content_type,
                executable: file.executable,
            })
        })
        .collect()
}

fn parse_kind(kind: &str) -> Result<ArtifactKind, HandlerError> {
    kind.parse::<ArtifactKind>()
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

fn parse_status(status: &str) -> Result<ArtifactStatus, HandlerError> {
    status
        .parse::<ArtifactStatus>()
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

fn artifact_registry() -> ArtifactRegistry {
    ArtifactRegistry::new(OrchestratorCtx::current().graph_pool.clone())
}

async fn authorized_write_scope(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    scope: MemoryScope,
) -> Result<MemoryScope, HandlerError> {
    if scope.is_global() {
        authorize_deployment_artifact_admin(ctx).await?;
        return Ok(MemoryScope::Global);
    }
    let identity = authorize_workspace(ctx, workspace_id, Relation::Editor).await?;
    checked_import_scope(workspace_id, scope, &identity).map_err(scope_handler_error)
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        relation,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

async fn authorize_deployment_artifact_admin(
    ctx: &impl RequestHeaders,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    if identity.identity_type != IdentityType::Service {
        return Err(TerminalError::new_with_code(
            403,
            "global artifact publish requires a service identity",
        )
        .into());
    }
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn reject_scope_workspace_mismatch(
    request_workspace_id: &WorkspaceId,
    scope: &MemoryScope,
) -> Result<(), HandlerError> {
    let Some(scope_workspace_id) = scope.workspace_id() else {
        return Ok(());
    };
    if &scope_workspace_id != request_workspace_id {
        return Err(scope_handler_error(SkillScopeError::WorkspaceMismatch {
            request_workspace_id: request_workspace_id.clone(),
            scope_workspace_id,
        }));
    }
    Ok(())
}

fn scope_handler_error(error: SkillScopeError) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
}

fn artifact_handler_error(error: MoaError) -> HandlerError {
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
