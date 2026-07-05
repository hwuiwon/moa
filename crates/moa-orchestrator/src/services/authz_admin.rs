//! Restate service for small authorization administration helpers.

use moa_authz::{enqueue_raw, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleOp};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Request body for writing one raw OpenFGA tuple through the outbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteTupleRequest {
    /// Tuple subject, such as `api_key:<id>`.
    pub user: String,
    /// Tuple relation.
    pub relation: String,
    /// Tuple object, such as `tenant:<id>`.
    pub object: String,
    /// Tenant id for outbox audit and admin authorization.
    pub tenant_id: Option<Uuid>,
}

/// Authorization administration service.
#[restate_sdk::service]
#[name = "Authz"]
pub trait Authz {
    /// Enqueue one tuple write after checking workspace or tenant admin access.
    async fn write_tuple(request: Json<WriteTupleRequest>) -> Result<(), HandlerError>;
}

/// Concrete authorization administration implementation.
#[derive(Clone, Default)]
pub struct AuthzImpl;

impl Authz for AuthzImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn write_tuple(
        &self,
        ctx: Context<'_>,
        request: Json<WriteTupleRequest>,
    ) -> Result<(), HandlerError> {
        annotate_restate_handler_span("Authz", "write_tuple");
        let identity = require_identity(&ctx)?;
        let request = request.into_inner();
        validate_tuple_wire(&request)?;
        let tenant_id = request
            .tenant_id
            .or_else(|| tenant_id_from_object(&request.object))
            .unwrap_or(identity.tenant_id.0);
        let fga = require_fga_client()?;
        authorize_tuple_write(&fga, &identity, &request, tenant_id).await?;
        let pool = OrchestratorCtx::current_graph_pool();

        Ok(ctx
            .run(|| async move {
                enqueue_raw(
                    &pool,
                    TupleOp::Write,
                    &request.user,
                    &request.relation,
                    &request.object,
                    Some(tenant_id),
                )
                .await
                .map_err(|error| TerminalError::new(format!("authz outbox: {error}")))?;
                Ok(())
            })
            .name("authz_write_tuple")
            .await?)
    }
}

async fn authorize_tuple_write(
    fga: &moa_authz::FgaClient,
    identity: &moa_core::traits::Identity,
    request: &WriteTupleRequest,
    tenant_id: Uuid,
) -> Result<(), HandlerError> {
    if let Some(workspace_id) = workspace_id_from_object(&request.object)? {
        return authorize_workspace_admin(fga, identity, workspace_id).await;
    }

    if let Some(workspace_id) = workspace_attachment_workspace_id(request)? {
        return authorize_workspace_admin(fga, identity, workspace_id).await;
    }

    require_authz_with_delegation(
        fga,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

async fn authorize_workspace_admin(
    fga: &moa_authz::FgaClient,
    identity: &moa_core::traits::Identity,
    workspace_id: Uuid,
) -> Result<(), HandlerError> {
    require_authz_with_delegation(
        fga,
        identity,
        ObjectType::Workspace,
        workspace_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

fn validate_tuple_wire(request: &WriteTupleRequest) -> Result<(), HandlerError> {
    if !request.user.contains(':') {
        return Err(TerminalError::new_with_code(400, "tuple user must be type:id").into());
    }
    if request.relation.trim().is_empty() {
        return Err(TerminalError::new_with_code(400, "tuple relation is required").into());
    }
    if !request.object.contains(':') {
        return Err(TerminalError::new_with_code(400, "tuple object must be type:id").into());
    }
    Ok(())
}

fn tenant_id_from_object(object: &str) -> Option<Uuid> {
    object
        .strip_prefix("tenant:")
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn workspace_id_from_object(object: &str) -> Result<Option<Uuid>, HandlerError> {
    let Some(value) = object.strip_prefix("workspace:") else {
        return Ok(None);
    };
    let workspace_id = Uuid::parse_str(value)
        .map_err(|_| TerminalError::new_with_code(400, "workspace id must be a uuid"))?;
    validate_canonical_workspace_id(workspace_id)?;
    Ok(Some(workspace_id))
}

fn workspace_attachment_workspace_id(
    request: &WriteTupleRequest,
) -> Result<Option<Uuid>, HandlerError> {
    if request.relation != "workspace" || tenant_id_from_object(&request.object).is_none() {
        return Ok(None);
    }
    let Some(value) = request.user.strip_prefix("workspace:") else {
        return Err(TerminalError::new_with_code(
            400,
            "tenant workspace tuple user must be workspace:<id>",
        )
        .into());
    };
    let workspace_id = Uuid::parse_str(value)
        .map_err(|_| TerminalError::new_with_code(400, "workspace id must be a uuid"))?;
    validate_canonical_workspace_id(workspace_id)?;
    Ok(Some(workspace_id))
}

fn validate_canonical_workspace_id(workspace_id: Uuid) -> Result<(), HandlerError> {
    if workspace_id != moa_core::WORKSPACE_ID {
        return Err(TerminalError::new_with_code(
            400,
            format!("workspace id must be {}", moa_core::WORKSPACE_ID),
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_object_id_is_parsed_for_admin_scoping() {
        // Pins: every canonical workspace object tuple is authorized against workspace#admin.
        let workspace_id = moa_core::WORKSPACE_ID;

        assert_eq!(
            workspace_id_from_object("workspace:00000000-0000-0000-0000-000000000001")
                .expect("canonical workspace object"),
            Some(workspace_id)
        );
        assert_eq!(
            workspace_id_from_object("tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
                .expect("non-workspace object"),
            None
        );
    }

    #[test]
    fn workspace_attachment_is_parsed_for_admin_scoping() {
        // Pins: canonical tenant workspace edges cannot be authorized as ordinary tenant tuple writes.
        let workspace_id = moa_core::WORKSPACE_ID;
        let request = WriteTupleRequest {
            user: "workspace:00000000-0000-0000-0000-000000000001".to_string(),
            relation: "workspace".to_string(),
            object: "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            tenant_id: None,
        };

        assert_eq!(
            workspace_attachment_workspace_id(&request).expect("canonical workspace attachment"),
            Some(workspace_id)
        );
    }

    #[test]
    fn non_canonical_workspace_id_is_rejected() {
        // Pins: MOA has one deployment workspace; tuple writes cannot create a second workspace.
        assert!(
            workspace_id_from_object("workspace:99999999-9999-9999-9999-999999999999").is_err()
        );
    }

    #[test]
    fn workspace_attachment_requires_workspace_user() {
        // Pins: tenant workspace edges cannot fall back to tenant-admin tuple authorization.
        let request = WriteTupleRequest {
            user: "user:11111111-1111-1111-1111-111111111111".to_string(),
            relation: "workspace".to_string(),
            object: "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            tenant_id: None,
        };

        assert!(workspace_attachment_workspace_id(&request).is_err());
    }

    #[test]
    fn non_workspace_attachment_is_not_admin_scoped() {
        // Pins: ordinary tenant admin/operator tuple writes still use tenant#admin.
        let request = WriteTupleRequest {
            user: "user:11111111-1111-1111-1111-111111111111".to_string(),
            relation: "admin".to_string(),
            object: "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_string(),
            tenant_id: None,
        };

        assert_eq!(
            workspace_attachment_workspace_id(&request).expect("ordinary tenant tuple"),
            None
        );
    }
}
