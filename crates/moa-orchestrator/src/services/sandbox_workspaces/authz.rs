//! Authorization gates for sandbox-workspace management handlers.

use moa_authz::fga_subject;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::{
    traits::Identity,
    types::{identifiers::SandboxWorkspaceId, sandbox_workspace::SandboxWorkspaceScope},
};
use restate_sdk::{
    context::{ContextSideEffects, RunFuture},
    prelude::{Context, HandlerError, Json, TerminalError},
};

use crate::handlers::authz_shim::{AuthzEnforcer, journal_context_authz, require_identity};

/// Authorizes creation under the verified durable owner scope.
pub(super) async fn authorize_create(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    scope: &SandboxWorkspaceScope,
) -> Result<Identity, HandlerError> {
    match scope {
        SandboxWorkspaceScope::Worker { session_id, .. } => {
            authz.authorize_session_participant(ctx, *session_id).await
        }
        SandboxWorkspaceScope::ExecutionTask { .. } => {
            let identity = require_identity(ctx)?;
            authz
                .authorize_tenant_operator_or_admin(ctx, identity.tenant_id)
                .await
        }
    }
}

/// Authorizes workspace use before any local workspace row is read.
pub(super) async fn authorize_use(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    workspace_id: SandboxWorkspaceId,
) -> Result<Identity, HandlerError> {
    authorize_workspace(authz, ctx, workspace_id, Relation::Use).await
}

/// Authorizes workspace management before any local workspace row is read.
pub(super) async fn authorize_manage(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    workspace_id: SandboxWorkspaceId,
) -> Result<Identity, HandlerError> {
    authorize_workspace(authz, ctx, workspace_id, Relation::Manage).await
}

async fn authorize_workspace(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
    workspace_id: SandboxWorkspaceId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    journal_context_authz(
        ctx,
        authz.require_fga_client()?,
        identity.clone(),
        ObjectType::SandboxWorkspace,
        workspace_id,
        relation,
    )
    .await?;
    Ok(identity)
}

/// Resolves and rechecks every workspace visible to the caller before DB listing.
pub(super) async fn authorized_workspace_ids(
    authz: &AuthzEnforcer,
    ctx: &Context<'_>,
) -> Result<(Identity, Vec<SandboxWorkspaceId>), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = authz.require_fga_client()?;
    let list_client = fga.clone();
    let subject = fga_subject(&identity);
    let objects = ctx
        .run(|| async move {
            list_client
                .list_objects("sandbox_workspace", "use", &subject)
                .await
                .map(Json::from)
                .map_err(|error| {
                    tracing::error!(%error, "list sandbox workspace authorization objects failed");
                    HandlerError::from(TerminalError::new_with_code(
                        503,
                        "authorization engine unavailable",
                    ))
                })
        })
        .name("sandbox_workspaces_list_authorized_objects")
        .await?
        .into_inner();

    let mut workspace_ids = Vec::with_capacity(objects.len());
    for object in objects {
        let Some(id) = object.strip_prefix("sandbox_workspace:") else {
            return Err(TerminalError::new_with_code(
                503,
                "authorization engine returned an invalid workspace object",
            )
            .into());
        };
        let workspace_id = id
            .parse::<uuid::Uuid>()
            .map(SandboxWorkspaceId)
            .map_err(|_| {
                TerminalError::new_with_code(
                    503,
                    "authorization engine returned an invalid workspace object",
                )
            })?;
        // ListObjects identifies candidates. This normal delegated check pins
        // both direct agent use and can_act_as before any tenant row is read.
        journal_context_authz(
            ctx,
            fga.clone(),
            identity.clone(),
            ObjectType::SandboxWorkspace,
            workspace_id,
            Relation::Use,
        )
        .await?;
        workspace_ids.push(workspace_id);
    }
    Ok((identity, workspace_ids))
}
