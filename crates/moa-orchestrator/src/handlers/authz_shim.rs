//! Authorization helpers shared by Restate handlers.

use std::fmt;
use std::time::Duration;

use moa_authz::{AuthzCheckError, FgaClient, require_authz_with_delegation};
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::types::identifiers::{SessionId, TenantId};
use restate_sdk::context::{ContextSideEffects, RunFuture};
use restate_sdk::prelude::{
    Context, HandlerError, ObjectContext, RunRetryPolicy, SharedObjectContext, TerminalError,
};
use restate_sdk::serde::Json;

use crate::ctx::{self, IdentityHeaderError, RequestHeaders};
use crate::workflows::errors::authz_check_error_to_handler_error;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum JournaledAuthzDecision {
    Allowed,
    Forbidden,
}

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
        ctx: &Context<'_>,
        tenant_id: TenantId,
        relation: Relation,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
        journal_context_authz(
            ctx,
            fga,
            identity.clone(),
            ObjectType::Tenant,
            tenant_id,
            relation,
        )
        .await?;
        Ok(identity)
    }

    /// Authorizes the caller as a participant of one parent session.
    pub async fn authorize_session_participant(
        &self,
        ctx: &Context<'_>,
        session_id: SessionId,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
        journal_context_authz(
            ctx,
            fga,
            identity.clone(),
            ObjectType::Session,
            session_id,
            Relation::Participant,
        )
        .await?;
        Ok(identity)
    }

    /// Authorizes tenant operators and admins for product control-plane work.
    pub async fn authorize_tenant_operator_or_admin(
        &self,
        ctx: &Context<'_>,
        tenant_id: TenantId,
    ) -> Result<Identity, HandlerError> {
        self.authorize_tenant(ctx, tenant_id, Relation::Operator)
            .await
    }

    /// Authorizes a virtual-object caller as a participant of one parent session.
    pub async fn authorize_object_session_participant(
        &self,
        ctx: &ObjectContext<'_>,
        session_id: SessionId,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
        journal_object_authz(
            ctx,
            fga,
            identity.clone(),
            ObjectType::Session,
            session_id,
            Relation::Participant,
        )
        .await?;
        Ok(identity)
    }

    /// Authorizes a shared virtual-object caller as a session participant.
    pub async fn authorize_shared_object_session_participant(
        &self,
        ctx: &SharedObjectContext<'_>,
        session_id: SessionId,
    ) -> Result<Identity, HandlerError> {
        let identity = require_identity(ctx)?;
        let fga = self.require_fga_client()?;
        journal_shared_object_authz(
            ctx,
            fga,
            identity.clone(),
            ObjectType::Session,
            session_id,
            Relation::Participant,
        )
        .await?;
        Ok(identity)
    }
}

/// Journals one authorization decision from a Restate service handler.
pub async fn journal_context_authz(
    ctx: &Context<'_>,
    fga: FgaClient,
    identity: Identity,
    object_type: ObjectType,
    object_id: impl fmt::Display,
    relation: Relation,
) -> Result<(), HandlerError> {
    let object_id = object_id.to_string();
    let action_name = authz_action_name(object_type, &object_id, relation);
    ctx.run(move || async move {
        require_authz_with_delegation(&fga, &identity, object_type, object_id, relation)
            .await
            .map_err(authz_run_error)
    })
    .name(action_name)
    .retry_policy(authz_retry_policy())
    .await
    .map_err(HandlerError::from)
}

/// Journals a primary authorization check with one explicit forbidden-only fallback.
#[allow(clippy::too_many_arguments)]
pub async fn journal_context_authz_any(
    ctx: &Context<'_>,
    fga: FgaClient,
    identity: Identity,
    primary_object_type: ObjectType,
    primary_object_id: impl fmt::Display,
    primary_relation: Relation,
    fallback_object_type: ObjectType,
    fallback_object_id: impl fmt::Display,
    fallback_relation: Relation,
) -> Result<(), HandlerError> {
    let primary_object_id = primary_object_id.to_string();
    let fallback_object_id = fallback_object_id.to_string();
    let primary_action_name =
        authz_action_name(primary_object_type, &primary_object_id, primary_relation);
    let primary_fga = fga.clone();
    let primary_identity = identity.clone();
    let decision = ctx
        .run(move || async move {
            match require_authz_with_delegation(
                &primary_fga,
                &primary_identity,
                primary_object_type,
                primary_object_id,
                primary_relation,
            )
            .await
            {
                Ok(()) => Ok(Json::from(JournaledAuthzDecision::Allowed)),
                Err(AuthzCheckError::Forbidden { .. }) => {
                    Ok(Json::from(JournaledAuthzDecision::Forbidden))
                }
                Err(engine @ AuthzCheckError::Engine(_)) => {
                    Err(authz_check_error_to_handler_error(engine))
                }
            }
        })
        .name(primary_action_name)
        .retry_policy(authz_retry_policy())
        .await?
        .into_inner();

    if matches!(decision, JournaledAuthzDecision::Allowed) {
        return Ok(());
    }

    journal_context_authz(
        ctx,
        fga,
        identity,
        fallback_object_type,
        fallback_object_id,
        fallback_relation,
    )
    .await
}

/// Journals one authorization decision from a Restate virtual-object handler.
pub async fn journal_object_authz(
    ctx: &ObjectContext<'_>,
    fga: FgaClient,
    identity: Identity,
    object_type: ObjectType,
    object_id: impl fmt::Display,
    relation: Relation,
) -> Result<(), HandlerError> {
    let object_id = object_id.to_string();
    let action_name = authz_action_name(object_type, &object_id, relation);
    ctx.run(move || async move {
        require_authz_with_delegation(&fga, &identity, object_type, object_id, relation)
            .await
            .map_err(authz_run_error)
    })
    .name(action_name)
    .retry_policy(authz_retry_policy())
    .await
    .map_err(HandlerError::from)
}

/// Journals one authorization decision from a shared virtual-object handler.
pub async fn journal_shared_object_authz(
    ctx: &SharedObjectContext<'_>,
    fga: FgaClient,
    identity: Identity,
    object_type: ObjectType,
    object_id: impl fmt::Display,
    relation: Relation,
) -> Result<(), HandlerError> {
    let object_id = object_id.to_string();
    let action_name = authz_action_name(object_type, &object_id, relation);
    ctx.run(move || async move {
        require_authz_with_delegation(&fga, &identity, object_type, object_id, relation)
            .await
            .map_err(authz_run_error)
    })
    .name(action_name)
    .retry_policy(authz_retry_policy())
    .await
    .map_err(HandlerError::from)
}

fn authz_action_name(object_type: ObjectType, object_id: &str, relation: Relation) -> String {
    format!("authz_check:{object_type}:{relation}:{object_id}")
}

fn authz_retry_policy() -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(100))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(1))
        .max_attempts(5)
        .max_duration(Duration::from_secs(5))
}

fn authz_run_error(error: AuthzCheckError) -> HandlerError {
    translate_authz_error(error)
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
            authz_check_error_to_handler_error(AuthzCheckError::Engine(error))
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
