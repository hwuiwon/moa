//! Authorization helpers shared by Restate handlers.

use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::types::identifiers::{SessionId, TenantId};
use restate_sdk::prelude::{HandlerError, TerminalError};

use crate::ctx::{self, IdentityHeaderError, RequestHeaders};

/// Explicit authorization dependency shared by Restate handler implementations.
#[derive(Clone)]
pub struct AuthzEnforcer {
    fga_client: Option<FgaClient>,
}

impl AuthzEnforcer {
    /// Creates an enforcer from the runtime's configured OpenFGA client.
    #[must_use]
    pub fn new(fga_client: Option<FgaClient>) -> Self {
        Self { fga_client }
    }

    /// Returns the configured FGA client or fails closed.
    pub fn require_fga_client(&self) -> Result<FgaClient, HandlerError> {
        require_configured_fga_client(self.fga_client.clone())
    }

    /// Authorizes the caller against a tenant for a specific relation.
    pub async fn authorize_tenant(
        &self,
        ctx: &impl RequestHeaders,
        tenant_id: TenantId,
        relation: Relation,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
        require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
            .await
            .map_err(translate_authz_error)?;
        Ok(identity)
    }

    /// Authorizes the caller as a participant of one parent session.
    pub async fn authorize_session_participant(
        &self,
        ctx: &impl RequestHeaders,
        session_id: SessionId,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
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

    /// Authorizes tenant operators and admins for product control-plane work.
    pub async fn authorize_tenant_operator_or_admin(
        &self,
        ctx: &impl RequestHeaders,
        tenant_id: TenantId,
    ) -> Result<Identity, HandlerError> {
        self.authorize_tenant(ctx, tenant_id, Relation::Operator)
            .await
    }
}

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

/// Return an explicitly configured FGA client or fail closed.
pub fn require_configured_fga_client(
    fga_client: Option<FgaClient>,
) -> Result<FgaClient, HandlerError> {
    fga_client
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
        headers.insert("x-moa-identity-type", "operator".to_string());
        let headers = TestHeaders(headers);

        let error = require_identity(&headers)
            .expect_err("partial identity headers should be rejected as malformed");

        assert_eq!(
            handler_error_text(error),
            "Terminal error [400]: bad identity: malformed identity header: partial identity headers; require all of type/id/tenant"
        );
    }
}
