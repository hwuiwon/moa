//! Restate service for canonical artifact import, export, validation, and publish requests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use moa_artifacts::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::registry::{
    ArtifactRegistry, NewArtifactDraft, NewArtifactFile, StoredArtifactRevision,
};
use moa_artifacts::release::ActivationTargetClass;
use moa_artifacts::resolver::ArtifactResolver;
use moa_artifacts::validation::validate_for_status;
use moa_authz_schema::Relation;
use moa_core::types::action_policy::ActionRuleScope;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::artifacts::{
    ArtifactExportRequest, ArtifactExportResponse, ArtifactFileDocument, ArtifactImportRequest,
    ArtifactImportResponse, ArtifactListRequest, ArtifactListResponse, ArtifactPublishRequest,
    ArtifactPublishResponse, ArtifactSummary, ArtifactValidateRequest, ArtifactValidateResponse,
};
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::AuthzEnforcer;
use crate::workflows::errors::moa_error_to_status_handler_error;

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

    /// Publishes a non-skill draft artifact revision.
    ///
    /// Learned skills activate only through `LearningReview/accept_skill`, whose
    /// regression evidence is unavailable on this generic authoring surface.
    async fn publish(
        request: Json<ArtifactPublishRequest>,
    ) -> Result<Json<ArtifactPublishResponse>, HandlerError>;
}

/// Concrete artifact service implementation.
#[derive(Clone)]
pub struct ArtifactsImpl {
    registry: ArtifactRegistry,
    authz: AuthzEnforcer,
}

impl ArtifactsImpl {
    /// Creates the artifact adapter with its persistent registry.
    #[must_use]
    pub fn new(registry: ArtifactRegistry, authz: AuthzEnforcer) -> Self {
        Self { registry, authz }
    }
}

impl Artifacts for ArtifactsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn import(
        &self,
        ctx: Context<'_>,
        request: Json<ArtifactImportRequest>,
    ) -> Result<Json<ArtifactImportResponse>, HandlerError> {
        annotate_restate_handler_span("Artifacts", "import");
        let request = request.into_inner();
        let scope = authorized_write_scope(&self.authz, &ctx, request.scope).await?;

        let registry = self.registry.clone();
        Ok(ctx
            .run(|| async move { import_inner(registry, scope, request).await.map(Json::from) })
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
        let scope = request.scope.unwrap_or(ActionRuleScope::Tenant {
            tenant_id: request.tenant_id,
        });
        authorize_read_scope(&self.authz, &ctx, &scope).await?;

        let registry = self.registry.clone();
        Ok(ctx
            .run(|| async move { export_inner(registry, scope, request).await.map(Json::from) })
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
        let scope = request.scope.unwrap_or(ActionRuleScope::Tenant {
            tenant_id: request.tenant_id,
        });
        authorize_read_scope(&self.authz, &ctx, &scope).await?;

        let registry = self.registry.clone();
        Ok(ctx
            .run(|| async move { list_inner(registry, scope, request).await.map(Json::from) })
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
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Operator)
            .await?;

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
        let scope = authorized_write_scope(&self.authz, &ctx, request.scope).await?;

        let registry = self.registry.clone();
        Ok(ctx
            .run(|| async move {
                publish_inner(registry, scope, request)
                    .await
                    .map(Json::from)
            })
            .name("artifacts_publish")
            .await?)
    }
}

async fn import_inner(
    registry: ArtifactRegistry,
    scope: ActionRuleScope,
    request: ArtifactImportRequest,
) -> Result<ArtifactImportResponse, HandlerError> {
    let document = parse_document(&request.source_format, &request.source_text)?;
    let files = decode_files(request.files)?;
    let stored = registry
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
        .map_err(moa_error_to_status_handler_error)?;

    Ok(ArtifactImportResponse {
        artifact_uid: stored.artifact_uid,
        revision_uid: stored.revision_uid,
        status: stored.status.to_string(),
        validation_report: stored.validation_report,
    })
}

async fn export_inner(
    registry: ArtifactRegistry,
    scope: ActionRuleScope,
    request: ArtifactExportRequest,
) -> Result<ArtifactExportResponse, HandlerError> {
    let kind = parse_kind(&request.kind)?;
    let stored = registry
        .load_visible(&scope, kind, &request.name)
        .await
        .map_err(moa_error_to_status_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "artifact not found"))?;
    let files = registry
        .load_files(&scope, stored.revision_uid)
        .await
        .map_err(moa_error_to_status_handler_error)?
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

async fn list_inner(
    registry: ArtifactRegistry,
    scope: ActionRuleScope,
    request: ArtifactListRequest,
) -> Result<ArtifactListResponse, HandlerError> {
    let kind = request.kind.as_deref().map(parse_kind).transpose()?;
    let status = request.status.as_deref().map(parse_status).transpose()?;
    let artifacts = registry
        .list_visible(&scope, kind, status)
        .await
        .map_err(moa_error_to_status_handler_error)?
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
    registry: ArtifactRegistry,
    scope: ActionRuleScope,
    request: ArtifactPublishRequest,
) -> Result<ArtifactPublishResponse, HandlerError> {
    let stored = registry
        .load_revision(&scope, request.revision_uid)
        .await
        .map_err(moa_error_to_status_handler_error)?
        .ok_or_else(|| TerminalError::new_with_code(404, "artifact revision not found"))?;
    if stored.kind == ArtifactKind::Skill {
        return Err(TerminalError::new_with_code(
            409,
            "skill revisions cannot be published manually; learned skills activate only through LearningReview/accept_skill",
        )
        .into());
    }
    let mut document = stored.document.clone();
    document.reference_resolutions = ArtifactResolver::new(registry.clone())
        .resolve_document(&scope, &document)
        .await
        .map_err(moa_error_to_status_handler_error)?;
    let report = validate_for_status(&document, ArtifactStatus::Published);
    if !report.is_ok() {
        return Err(
            TerminalError::new_with_code(400, "artifact revision failed validation").into(),
        );
    }

    let stored = if ActivationTargetClass::is_release_gated(&stored.kind) {
        registry
            .record_validation_report(&scope, request.revision_uid, &report)
            .await
            .map_err(moa_error_to_status_handler_error)?
    } else {
        registry
            .publish_unserved_revision(&scope, request.revision_uid, &report)
            .await
            .map_err(moa_error_to_status_handler_error)?
    };
    let validation_report = serde_json::to_value(report)
        .map_err(|error| TerminalError::new_with_code(500, error.to_string()))?;
    Ok(publish_response(stored, validation_report))
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

async fn authorized_write_scope(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    scope: ActionRuleScope,
) -> Result<ActionRuleScope, HandlerError> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
            authz
                .authorize_tenant(ctx, tenant_id, Relation::Operator)
                .await?;
        }
    }
    Ok(scope)
}

async fn authorize_read_scope(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    scope: &ActionRuleScope,
) -> Result<(), HandlerError> {
    match scope {
        ActionRuleScope::Tenant { tenant_id } | ActionRuleScope::Contact { tenant_id, .. } => {
            authz
                .authorize_tenant(ctx, *tenant_id, Relation::Operator)
                .await?;
        }
    }
    Ok(())
}
