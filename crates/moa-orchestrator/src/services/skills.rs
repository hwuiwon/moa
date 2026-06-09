//! Restate service for cloud-owned skill import, export, listing, and bootstrap requests.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::{
    SkillBootstrapGlobalRequest, SkillBootstrapGlobalResponse, SkillExportRequest,
    SkillExportResponse, SkillImportDocument, SkillImportRequest, SkillImportResponse,
    SkillListRequest, SkillListResponse, SkillSummary,
};
use moa_core::{MemoryScope, MoaError, UserId, WorkspaceId};
use moa_skills::{NewSkill, Skill, SkillRegistry, parse_skill_markdown};
use restate_sdk::prelude::*;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Restate service surface for protected skill operations.
#[restate_sdk::service]
#[name = "Skills"]
pub trait Skills {
    /// Exports visible workspace skills after a workspace member check.
    async fn export(
        request: Json<SkillExportRequest>,
    ) -> Result<Json<SkillExportResponse>, HandlerError>;

    /// Imports global, workspace, or user scoped skill documents after the matching authz check.
    async fn import(
        request: Json<SkillImportRequest>,
    ) -> Result<Json<SkillImportResponse>, HandlerError>;

    /// Lists visible workspace skills after a workspace member check.
    async fn list(request: Json<SkillListRequest>)
    -> Result<Json<SkillListResponse>, HandlerError>;

    /// Imports deployment-global skill documents after a service-operator check.
    async fn bootstrap_global(
        request: Json<SkillBootstrapGlobalRequest>,
    ) -> Result<Json<SkillBootstrapGlobalResponse>, HandlerError>;
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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

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
        reject_scope_workspace_mismatch(&request.workspace_id, &request.scope)?;
        let scope = if request.scope.is_global() {
            authorize_deployment_skill_admin(&ctx).await?;
            MemoryScope::Global
        } else {
            let identity =
                authorize_workspace(&ctx, &request.workspace_id, Relation::Editor).await?;
            checked_import_scope(&request.workspace_id, request.scope, &identity)
                .map_err(skill_scope_handler_error)?
        };
        let documents = request.documents;

        Ok(ctx
            .run(|| async move { import_inner(scope, documents).await.map(Json::from) })
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
        authorize_workspace(&ctx, &request.workspace_id, Relation::Member).await?;

        Ok(ctx
            .run(|| async move { list_inner(request).await.map(Json::from) })
            .name("skills_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn bootstrap_global(
        &self,
        ctx: Context<'_>,
        request: Json<SkillBootstrapGlobalRequest>,
    ) -> Result<Json<SkillBootstrapGlobalResponse>, HandlerError> {
        annotate_restate_handler_span("Skills", "bootstrap_global");
        authorize_deployment_skill_admin(&ctx).await?;
        let documents = request.into_inner().documents;

        Ok(ctx
            .run(|| async move {
                let response = import_inner(MemoryScope::Global, documents).await?;
                Ok(Json(SkillBootstrapGlobalResponse {
                    imported: response.imported,
                }))
            })
            .name("skills_bootstrap_global")
            .await?)
    }
}

/// Skill import scope validation error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SkillScopeError {
    /// The request authorized one workspace but asked to write another.
    #[error(
        "skill import scope workspace {scope_workspace_id} does not match request workspace {request_workspace_id}"
    )]
    WorkspaceMismatch {
        /// Workspace used for authorization.
        request_workspace_id: WorkspaceId,
        /// Workspace embedded in the requested scope.
        scope_workspace_id: WorkspaceId,
    },
    /// The caller requested a user id that does not match the trusted identity.
    #[error("requested user_id {requested} does not match trusted caller user {effective}")]
    UserMismatch {
        /// User id supplied by the request scope.
        requested: UserId,
        /// User id derived from the trusted identity, when one exists.
        effective: String,
    },
}

/// Returns the user id represented by a trusted identity or agent delegation.
#[must_use]
pub fn effective_user_id(identity: &Identity) -> Option<UserId> {
    match identity.identity_type {
        IdentityType::User => Some(UserId::new(identity.id.to_string())),
        IdentityType::Agent => identity
            .acting_on_behalf_of
            .map(|user_id| UserId::new(user_id.to_string())),
        IdentityType::Service => None,
    }
}

/// Validates an import scope against the authorized workspace and trusted identity.
pub fn checked_import_scope(
    request_workspace_id: &WorkspaceId,
    scope: MemoryScope,
    identity: &Identity,
) -> Result<MemoryScope, SkillScopeError> {
    match scope {
        MemoryScope::Global => Ok(MemoryScope::Global),
        MemoryScope::Workspace { workspace_id } => {
            if &workspace_id != request_workspace_id {
                return Err(SkillScopeError::WorkspaceMismatch {
                    request_workspace_id: request_workspace_id.clone(),
                    scope_workspace_id: workspace_id,
                });
            }
            Ok(MemoryScope::Workspace { workspace_id })
        }
        MemoryScope::User {
            workspace_id,
            user_id,
        } => {
            if &workspace_id != request_workspace_id {
                return Err(SkillScopeError::WorkspaceMismatch {
                    request_workspace_id: request_workspace_id.clone(),
                    scope_workspace_id: workspace_id,
                });
            }
            let effective = effective_user_id(identity);
            if effective.as_ref() != Some(&user_id) {
                return Err(SkillScopeError::UserMismatch {
                    requested: user_id,
                    effective: effective
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "<none>".to_string()),
                });
            }
            Ok(MemoryScope::User {
                workspace_id,
                user_id,
            })
        }
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
        created_at: skill.created_at,
        updated_at: skill.updated_at,
    })
}

async fn export_inner(request: SkillExportRequest) -> Result<SkillExportResponse, HandlerError> {
    let workspace_id = request.workspace_id;
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let registry = skill_registry();
    let skills = registry
        .load_for_scope(&scope)
        .await
        .map_err(skill_handler_error)?;
    let documents = skills
        .into_iter()
        .map(skill_import_document_from_skill)
        .collect();
    Ok(SkillExportResponse {
        workspace_id,
        documents,
    })
}

async fn import_inner(
    scope: MemoryScope,
    documents: Vec<SkillImportDocument>,
) -> Result<SkillImportResponse, HandlerError> {
    let registry = skill_registry();
    let mut imported = 0_u64;
    for document in documents {
        let parsed = parse_skill_markdown(&document.body).map_err(skill_handler_error)?;
        let skill = NewSkill::from_document(scope.clone(), &parsed, document.body);
        registry
            .upsert_by_name(skill)
            .await
            .map_err(skill_handler_error)?;
        imported = imported.saturating_add(1);
    }
    Ok(SkillImportResponse { scope, imported })
}

async fn list_inner(request: SkillListRequest) -> Result<SkillListResponse, HandlerError> {
    let scope = MemoryScope::Workspace {
        workspace_id: request.workspace_id,
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
    SkillRegistry::new(OrchestratorCtx::current().graph_pool.clone())
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

fn reject_scope_workspace_mismatch(
    request_workspace_id: &WorkspaceId,
    scope: &MemoryScope,
) -> Result<(), HandlerError> {
    let Some(scope_workspace_id) = scope.workspace_id() else {
        return Ok(());
    };
    if &scope_workspace_id != request_workspace_id {
        return Err(skill_scope_handler_error(
            SkillScopeError::WorkspaceMismatch {
                request_workspace_id: request_workspace_id.clone(),
                scope_workspace_id,
            },
        ));
    }
    Ok(())
}

async fn authorize_deployment_skill_admin(ctx: &impl RequestHeaders) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    if identity.identity_type != IdentityType::Service {
        return Err(TerminalError::new_with_code(
            403,
            "global skill import requires a service identity",
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

fn skill_import_document_from_skill(skill: Skill) -> SkillImportDocument {
    SkillImportDocument {
        name: Some(skill.name),
        description: skill.description,
        body: skill.body,
        source_uri: None,
        metadata: serde_json::json!({
            "skill_uid": skill.skill_uid,
            "version": skill.version,
        }),
    }
}

fn memory_scope_from_skill(skill: &Skill) -> Result<MemoryScope, HandlerError> {
    match skill.scope.as_str() {
        "global" => Ok(MemoryScope::Global),
        "workspace" => skill
            .workspace_id
            .clone()
            .map(|workspace_id| MemoryScope::Workspace { workspace_id })
            .ok_or_else(|| {
                TerminalError::new_with_code(500, "workspace skill row missing workspace_id").into()
            }),
        "user" => match (skill.workspace_id.clone(), skill.user_id.clone()) {
            (Some(workspace_id), Some(user_id)) => Ok(MemoryScope::User {
                workspace_id,
                user_id,
            }),
            _ => Err(TerminalError::new_with_code(500, "user skill row missing scope ids").into()),
        },
        other => {
            Err(TerminalError::new_with_code(500, format!("unknown skill scope `{other}`")).into())
        }
    }
}

fn skill_scope_handler_error(error: SkillScopeError) -> HandlerError {
    TerminalError::new_with_code(400, error.to_string()).into()
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
