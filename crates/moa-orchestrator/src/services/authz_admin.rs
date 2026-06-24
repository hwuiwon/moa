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
    /// Enqueue one tuple write after checking tenant admin access.
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
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            tenant_id,
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;
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
