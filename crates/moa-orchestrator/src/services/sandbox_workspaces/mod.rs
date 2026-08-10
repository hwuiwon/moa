//! Authorized Restate service for tenant sandbox-workspace management.

mod authz;
mod management;

use moa_authz::FgaClient;
use moa_core::types::identifiers::WorkspaceOperationId;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::sandbox_workspaces::{
    CreateSandboxWorkspaceRequest, RestoreSandboxWorkspaceRequest, SandboxWorkspaceIdRequest,
    SandboxWorkspaceListRequest, SandboxWorkspaceListResponse, SandboxWorkspaceSummary,
};
use restate_sdk::prelude::*;

use crate::handlers::authz_shim::AuthzEnforcer;

use self::authz::{authorize_create, authorize_manage, authorize_use, authorized_workspace_ids};
pub(crate) use self::management::SandboxWorkspaceManagement;

/// Restate service surface for durable sandbox-workspace management.
#[restate_sdk::service]
#[name = "SandboxWorkspaces"]
pub trait SandboxWorkspaces {
    /// Creates a workspace under one verified worker or execution-task scope.
    async fn create(
        request: Json<CreateSandboxWorkspaceRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;

    /// Lists workspaces on which the caller has `use`.
    async fn list(
        request: Json<SandboxWorkspaceListRequest>,
    ) -> Result<Json<SandboxWorkspaceListResponse>, HandlerError>;

    /// Loads one workspace after a `use` authorization check.
    async fn get(
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;

    /// Authorizes an attach request and verifies the local lifecycle fence.
    async fn attach(
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;

    /// Authorizes a checkpoint request and verifies the local lifecycle fence.
    async fn checkpoint(
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;

    /// Authorizes a restore request and verifies the local lifecycle fence.
    async fn restore(
        request: Json<RestoreSandboxWorkspaceRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;

    /// Fences a workspace and enqueues inverse authorization tuples atomically.
    async fn delete(
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError>;
}

/// Concrete sandbox-workspace Restate service.
#[derive(Clone)]
pub struct SandboxWorkspacesImpl {
    management: SandboxWorkspaceManagement,
    authz: AuthzEnforcer,
}

impl SandboxWorkspacesImpl {
    /// Builds the workspace service from runtime persistence and authorization dependencies.
    #[must_use]
    pub(crate) fn new(
        management: SandboxWorkspaceManagement,
        fga_client: Option<FgaClient>,
    ) -> Self {
        Self {
            management,
            authz: AuthzEnforcer::new(fga_client),
        }
    }
}

impl SandboxWorkspaces for SandboxWorkspacesImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create(
        &self,
        ctx: Context<'_>,
        request: Json<CreateSandboxWorkspaceRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "create");
        self.management.require_admission_mode()?;
        let request = request.into_inner();
        let identity = authorize_create(&self.authz, &ctx, &request.scope).await?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move { management.create(identity, request).await.map(Json::from) })
            .name("sandbox_workspaces_create")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list(
        &self,
        ctx: Context<'_>,
        request: Json<SandboxWorkspaceListRequest>,
    ) -> Result<Json<SandboxWorkspaceListResponse>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "list");
        self.management.require_management()?;
        let _request = request.into_inner();
        let (identity, workspace_ids) = authorized_workspace_ids(&self.authz, &ctx).await?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .list(identity, workspace_ids)
                    .await
                    .map(|workspaces| Json::from(SandboxWorkspaceListResponse { workspaces }))
            })
            .name("sandbox_workspaces_list")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn get(
        &self,
        ctx: Context<'_>,
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "get");
        self.management.require_management()?;
        self.authorize_and_load(&ctx, request.into_inner()).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn attach(
        &self,
        ctx: Context<'_>,
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "attach");
        self.management.require_admission_mode()?;
        let request = request.into_inner();
        let identity = authorize_use(&self.authz, &ctx, request.workspace_id).await?;
        self.management.require_admission(identity.tenant_id)?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .attach(identity, request.workspace_id)
                    .await
                    .map(Json::from)
            })
            .name("sandbox_workspaces_attach")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn checkpoint(
        &self,
        ctx: Context<'_>,
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "checkpoint");
        self.management.require_admission_mode()?;
        let request = request.into_inner();
        let identity = authorize_use(&self.authz, &ctx, request.workspace_id).await?;
        self.management.require_admission(identity.tenant_id)?;
        let operation_id =
            management_operation_id(request.workspace_id, "checkpoint", ctx.invocation_id());
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .checkpoint(identity, request.workspace_id, operation_id)
                    .await
                    .map(Json::from)
            })
            .name("sandbox_workspaces_checkpoint")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn restore(
        &self,
        ctx: Context<'_>,
        request: Json<RestoreSandboxWorkspaceRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "restore");
        self.management.require_admission_mode()?;
        let request = request.into_inner();
        let identity = authorize_use(&self.authz, &ctx, request.workspace_id).await?;
        self.management.require_admission(identity.tenant_id)?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .restore(identity, request.workspace_id, request.checkpoint_id)
                    .await
                    .map(Json::from)
            })
            .name("sandbox_workspaces_restore")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn delete(
        &self,
        ctx: Context<'_>,
        request: Json<SandboxWorkspaceIdRequest>,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        annotate_restate_handler_span("SandboxWorkspaces", "delete");
        self.management.require_management()?;
        let request = request.into_inner();
        let identity = authorize_manage(&self.authz, &ctx, request.workspace_id).await?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .delete(identity, request.workspace_id)
                    .await
                    .map(Json::from)
            })
            .name("sandbox_workspaces_delete")
            .await?)
    }
}

fn management_operation_id(
    workspace_id: moa_core::types::identifiers::SandboxWorkspaceId,
    operation: &str,
    invocation_id: &str,
) -> WorkspaceOperationId {
    WorkspaceOperationId(uuid::Uuid::new_v5(
        &workspace_id.0,
        format!("public-workspace-{operation}-v1:{invocation_id}").as_bytes(),
    ))
}

impl SandboxWorkspacesImpl {
    async fn authorize_and_load(
        &self,
        ctx: &Context<'_>,
        request: SandboxWorkspaceIdRequest,
    ) -> Result<Json<SandboxWorkspaceSummary>, HandlerError> {
        let identity = authorize_use(&self.authz, ctx, request.workspace_id).await?;
        let management = self.management.clone();
        Ok(ctx
            .run(|| async move {
                management
                    .get_accessible(identity, request.workspace_id)
                    .await
                    .map(Json::from)
            })
            .name("sandbox_workspaces_load_authorized")
            .await?)
    }
}
