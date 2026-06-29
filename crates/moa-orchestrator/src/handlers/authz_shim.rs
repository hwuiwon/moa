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
        Err(IdentityHeaderError::Missing(_)) => {
            Err(TerminalError::new_with_code(401, "identity required").into())
        }
        Err(error) => Err(translate_identity_error(error)),
    }
}

/// Return the process-wide FGA client or fail closed.
pub fn require_fga_client() -> Result<FgaClient, HandlerError> {
    OrchestratorCtx::current()
        .fga_client()
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

#[cfg(test)]
mod tests {
    use restate_sdk::prelude::{HandlerError, HeaderMap};

    use super::*;

    struct TestHeaders(HeaderMap);

    impl RequestHeaders for TestHeaders {
        fn request_headers(&self) -> &HeaderMap {
            &self.0
        }
    }

    fn handler_error_text(error: HandlerError) -> String {
        let error_ref =
            <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(&error);
        error_ref.to_string()
    }

    #[test]
    fn require_identity_maps_absent_identity_to_unauthorized() {
        // Pins: protected Restate handlers report missing edge identity as authn failure.
        let headers = TestHeaders(HeaderMap::with_capacity(0));

        let error =
            require_identity(&headers).expect_err("missing identity should be unauthorized");

        assert_eq!(
            handler_error_text(error),
            "Terminal error [401]: identity required"
        );
    }

    #[test]
    fn require_identity_keeps_partial_identity_as_bad_request() {
        // Pins: forged or truncated trusted identity headers are malformed requests.
        let mut headers = HeaderMap::with_capacity(1);
        headers.insert("x-moa-identity-type", "user".to_string());
        let headers = TestHeaders(headers);

        let error = require_identity(&headers)
            .expect_err("partial identity headers should be rejected as malformed");

        assert_eq!(
            handler_error_text(error),
            "Terminal error [400]: bad identity: malformed identity header: partial identity headers; require all of type/id/tenant"
        );
    }
}
