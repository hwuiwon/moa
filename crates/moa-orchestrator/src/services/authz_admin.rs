//! Restate service for small authorization administration helpers.

use moa_authz::{enqueue, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_core::TenantId;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Tenant role relation that public API-key authz administration can write.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyTenantRole {
    /// Tenant administrator.
    Admin,
    /// Tenant operator.
    Operator,
}

impl ApiKeyTenantRole {
    fn relation(self) -> Relation {
        match self {
            Self::Admin => Relation::Admin,
            Self::Operator => Relation::Operator,
        }
    }
}

/// Request body for one typed public authorization tuple operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WriteTupleRequest {
    /// Grant an API key a tenant admin/operator role.
    GrantApiKeyTenantRole {
        /// API-key subject id.
        api_key_id: Uuid,
        /// Tenant object id.
        tenant_id: Uuid,
        /// Tenant role relation to grant.
        relation: ApiKeyTenantRole,
    },
    /// Revoke an API key tenant admin/operator role.
    RevokeApiKeyTenantRole {
        /// API-key subject id.
        api_key_id: Uuid,
        /// Tenant object id.
        tenant_id: Uuid,
        /// Tenant role relation to revoke.
        relation: ApiKeyTenantRole,
    },
}

impl WriteTupleRequest {
    fn tuple_op(&self) -> TupleOp {
        match self {
            Self::GrantApiKeyTenantRole { .. } => TupleOp::Write,
            Self::RevokeApiKeyTenantRole { .. } => TupleOp::Delete,
        }
    }

    fn api_key_id(&self) -> Uuid {
        match self {
            Self::GrantApiKeyTenantRole { api_key_id, .. }
            | Self::RevokeApiKeyTenantRole { api_key_id, .. } => *api_key_id,
        }
    }

    fn tenant_id(&self) -> Uuid {
        match self {
            Self::GrantApiKeyTenantRole { tenant_id, .. }
            | Self::RevokeApiKeyTenantRole { tenant_id, .. } => *tenant_id,
        }
    }

    fn relation(&self) -> Relation {
        match self {
            Self::GrantApiKeyTenantRole { relation, .. }
            | Self::RevokeApiKeyTenantRole { relation, .. } => relation.relation(),
        }
    }

    fn tuple_key(&self, object_tenant_id: Uuid) -> TupleKey {
        TupleKey::new(
            UserType::ApiKey,
            self.api_key_id(),
            self.relation(),
            ObjectType::Tenant,
            object_tenant_id,
        )
    }
}

/// Authorization administration service.
#[restate_sdk::service]
#[name = "Authz"]
pub trait Authz {
    /// Enqueue one typed public tuple write after tenant-admin authorization.
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
        let pool = OrchestratorCtx::current_graph_pool();
        let load_pool = pool.clone();
        let load_request = request.clone();
        let object_tenant_id = ctx
            .run(|| async move {
                load_api_key_tenant_for_tuple(load_pool, &load_request)
                    .await
                    .map(Json::from)
            })
            .name("authz_load_api_key_tenant")
            .await?
            .into_inner();
        let fga = require_fga_client()?;
        require_authz_with_delegation(
            &fga,
            &identity,
            ObjectType::Tenant,
            TenantId::from(object_tenant_id),
            Relation::Admin,
        )
        .await
        .map_err(translate_authz_error)?;

        Ok(ctx
            .run(|| async move {
                enqueue_typed_tuple_write_for_tenant(pool, request, object_tenant_id).await
            })
            .name("authz_write_typed_tuple")
            .await?)
    }
}

/// Load and verify the target API-key tenant for a typed tuple write.
pub async fn load_api_key_tenant_for_tuple(
    pool: PgPool,
    request: &WriteTupleRequest,
) -> Result<Uuid, HandlerError> {
    let api_key_id = request.api_key_id();
    let stored_tenant_id: Option<Uuid> = sqlx::query_scalar(
        r#"
        SELECT tenant_id
        FROM api_keys
        WHERE id = $1 AND revoked_at IS NULL
        "#,
    )
    .bind(api_key_id)
    .fetch_optional(&pool)
    .await
    .map_err(|error| TerminalError::new(format!("load api key for authz tuple: {error}")))?;
    let Some(stored_tenant_id) = stored_tenant_id else {
        return Err(TerminalError::new_with_code(404, "API key not found").into());
    };
    if stored_tenant_id != request.tenant_id() {
        return Err(TerminalError::new_with_code(403, "API key tenant mismatch").into());
    }
    Ok(stored_tenant_id)
}

/// Enqueue one typed public tuple write after validating API-key ownership.
pub async fn enqueue_typed_tuple_write(
    pool: PgPool,
    request: WriteTupleRequest,
) -> Result<(), HandlerError> {
    let object_tenant_id = load_api_key_tenant_for_tuple(pool.clone(), &request).await?;
    enqueue_typed_tuple_write_for_tenant(pool, request, object_tenant_id).await
}

async fn enqueue_typed_tuple_write_for_tenant(
    pool: PgPool,
    request: WriteTupleRequest,
    object_tenant_id: Uuid,
) -> Result<(), HandlerError> {
    let tuple = request.tuple_key(object_tenant_id);
    let op = request.tuple_op();
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;

    enqueue(&mut *transaction, op, &tuple, Some(object_tenant_id))
        .await
        .map_err(|error| TerminalError::new(format!("authz outbox: {error}")))?;
    transaction
        .commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_role_tuple_is_object_owned_by_tenant() {
        // Pins: the tenant object, not caller-supplied audit metadata, determines admin scope.
        let api_key_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("fixture API key UUID should parse");
        let tenant_id = Uuid::parse_str("22222222-2222-2222-2222-222222222222")
            .expect("fixture tenant UUID should parse");
        let request = WriteTupleRequest::GrantApiKeyTenantRole {
            api_key_id,
            tenant_id,
            relation: ApiKeyTenantRole::Admin,
        };

        assert_eq!(request.tuple_op(), TupleOp::Write);
        assert_eq!(
            request.tuple_key(tenant_id),
            TupleKey::new(
                UserType::ApiKey,
                api_key_id,
                Relation::Admin,
                ObjectType::Tenant,
                tenant_id,
            )
        );
    }

    #[test]
    fn api_key_role_revoke_uses_delete_operation() {
        // Pins: grant and revoke share the same typed tuple key but distinct outbox operations.
        let request = WriteTupleRequest::RevokeApiKeyTenantRole {
            api_key_id: Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                .expect("fixture API key UUID should parse"),
            tenant_id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                .expect("fixture tenant UUID should parse"),
            relation: ApiKeyTenantRole::Operator,
        };

        assert_eq!(request.tuple_op(), TupleOp::Delete);
        assert_eq!(request.relation(), Relation::Operator);
    }
}
