//! Authorization helpers for analytics service handlers.

use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::TenantId;
use moa_core::traits::IdentityType;
use restate_sdk::prelude::*;

use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};

/// Requires participant access to a session before reading session analytics.
pub(super) async fn authorize_session_participant(
    ctx: &impl RequestHeaders,
    session_id: moa_core::SessionId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)
}

/// Requires tenant operator access before reading tenant analytics.
pub(super) async fn authorize_tenant_member(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    .map_err(translate_authz_error)
}

/// Requires tenant admin access before reading admin-only analytics.
pub(super) async fn authorize_tenant_admin(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .map_err(translate_authz_error)
}

/// Requires a service identity authorized as a deployment operator.
pub(super) async fn authorize_deployment_operator(
    ctx: &impl RequestHeaders,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    if identity.identity_type != IdentityType::Service {
        return Err(TerminalError::new_with_code(
            403,
            "deployment-wide tool stats require a service identity",
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
