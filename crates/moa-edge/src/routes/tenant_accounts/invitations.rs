//! Tenant invitation creation and acceptance routes.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};

use crate::tenant_accounts::{AcceptTenantInvitationRequest, InviteTenantUserRequest, application};

use super::super::auth_accounts::{
    issue_login_session, normalize_email, validate_password_policy, validate_settings,
};
use super::super::{AppState, authenticate_direct_request, parse_json_body, require_direct_authz};
use super::{application_error_response, looks_like_email};

/// Invite a tenant admin or operator to set up their own account.
#[tracing::instrument(skip(state, headers, body))]
pub(crate) async fn invite_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity =
        match authenticate_direct_request(&state, &headers, "/v1/tenant/invitations").await {
            Ok(identity) => identity,
            Err(response) => return response,
        };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Admin,
    )
    .await
    {
        return response;
    }
    let request: InviteTenantUserRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.email);
    if !looks_like_email(&email) {
        return (StatusCode::BAD_REQUEST, "email must be an email address").into_response();
    }
    let tenant = match application::load_tenant(&state, identity.tenant_id.0).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => return application_error_response(error),
    };
    let invitation = match application::create_invitation(
        &state,
        identity.tenant_id.0,
        identity.id,
        tenant.name,
        request,
        email,
    )
    .await
    {
        Ok(invitation) => invitation,
        Err(error) => return application_error_response(error),
    };
    let mut response = invitation.response;
    response.delivery_sent = match application::deliver_invitation(
        &state,
        &response,
        &invitation.tenant_name,
        &invitation.token,
    )
    .await
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                error = %error,
                tenant_id = %response.tenant_id,
                user_id = %response.user_id,
                invitation_id = %response.id,
                "tenant invitation delivery failed"
            );
            false
        }
    };
    (StatusCode::CREATED, Json(response)).into_response()
}

/// Accept a tenant invitation, set the user's password, and sign in.
#[tracing::instrument(skip(state, body))]
// SAFETY: Invitation acceptance is authorized by consuming a short-lived one-time bearer token for one tenant user.
pub(crate) async fn accept_invitation(State(state): State<AppState>, body: Bytes) -> Response {
    let request: AcceptTenantInvitationRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.password) {
        return response;
    }
    let token = request.token.trim();
    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "invitation token is required").into_response();
    }
    let token_hash = application::invitation_token_hash(token);
    let credential = match application::accept_invitation(&state, request, token_hash).await {
        Ok(credential) => credential,
        Err(error) => return application_error_response(error),
    };
    match issue_login_session(&state, credential, true).await {
        Ok(session) => session.into_response(),
        Err(response) => response,
    }
}
