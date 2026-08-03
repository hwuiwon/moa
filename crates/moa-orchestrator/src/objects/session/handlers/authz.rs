//! Authorization helpers shared by Session virtual-object handlers.

use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{AuthzEnforcer, require_identity, translate_authz_error};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::types::identifiers::SessionId;
use restate_sdk::prelude::HandlerError;

/// Requires the authenticated caller to participate in the target session.
pub(super) async fn require_session_participant(
    authz: &AuthzEnforcer,
    ctx: &impl RequestHeaders,
    session_id: SessionId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = authz.require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Session,
        session_id,
        Relation::Participant,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}
