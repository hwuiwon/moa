//! Authorization helpers shared by Restate handlers.

use moa_authz::{AuthzCheckError, FgaClient};
use moa_core::traits::Identity;
use restate_sdk::prelude::{HandlerError, TerminalError};

use crate::ctx::{self, IdentityHeaderError, OrchestratorCtx, RequestHeaders};

/// Load the required caller identity from a Restate context.
pub fn require_identity(ctx: &impl RequestHeaders) -> Result<Identity, HandlerError> {
    match ctx::current_identity(ctx) {
        Ok(Some(identity)) => Ok(identity),
        Ok(None) => Err(TerminalError::new_with_code(401, "identity required").into()),
        Err(error) => Err(translate_identity_error(error)),
    }
}

/// Return the process-wide FGA client or fail closed.
pub fn require_fga_client() -> Result<FgaClient, HandlerError> {
    OrchestratorCtx::current()
        .fga_client
        .clone()
        .ok_or_else(|| TerminalError::new_with_code(503, "authorization engine unavailable").into())
}

/// Translate identity-header failures into handler errors.
pub fn translate_identity_error(error: IdentityHeaderError) -> HandlerError {
    tracing::info!(error = %error, "invalid identity headers");
    TerminalError::new_with_code(400, format!("bad identity: {error}")).into()
}

/// Translate authorization-check failures into handler errors.
pub fn translate_authz_error(error: AuthzCheckError) -> HandlerError {
    match error {
        AuthzCheckError::Forbidden {
            subject,
            object_type,
            object_id,
            relation,
        } => {
            tracing::info!(
                deny.subject = %subject,
                deny.object = format!("{object_type}:{object_id}"),
                deny.relation = %relation,
                "authz denied"
            );
            TerminalError::new_with_code(
                403,
                format!("forbidden: {subject} not {relation} on {object_type}:{object_id}"),
            )
            .into()
        }
        AuthzCheckError::Engine(error) => {
            tracing::error!(error = %error, "authz engine error; failing closed");
            TerminalError::new_with_code(503, "authorization engine unavailable").into()
        }
    }
}
