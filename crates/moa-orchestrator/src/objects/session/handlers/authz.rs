//! Authorization helpers shared by Session virtual-object handlers.

use crate::handlers::authz_shim::AuthzEnforcer;
use moa_core::types::identifiers::SessionId;
use restate_sdk::prelude::{HandlerError, ObjectContext, SharedObjectContext};

/// Requires the authenticated caller to participate in the target session.
pub(super) async fn require_session_participant(
    authz: &AuthzEnforcer,
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    authz
        .authorize_object_session_participant(ctx, session_id)
        .await
}

/// Requires a shared handler caller to participate in the target session.
pub(super) async fn require_shared_session_participant(
    authz: &AuthzEnforcer,
    ctx: &SharedObjectContext<'_>,
    session_id: SessionId,
) -> Result<moa_core::traits::Identity, HandlerError> {
    authz
        .authorize_shared_object_session_participant(ctx, session_id)
        .await
}
