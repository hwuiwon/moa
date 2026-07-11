//! Tenant settings and user-management routes.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};

use crate::tenant_accounts::{CreateTenantUserRequest, PatchTenantRequest, application};

use super::super::auth_accounts::{normalize_email, validate_password_policy, validate_settings};
use super::super::{AppState, authenticate_direct_request, parse_json_body, require_direct_authz};
use super::{application_error_response, looks_like_email};

/// Return the authenticated caller's tenant account.
#[tracing::instrument(skip(state, headers))]
pub(crate) async fn get_tenant(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant").await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if let Err(response) = require_direct_authz(
        &state,
        &identity,
        ObjectType::Tenant,
        identity.tenant_id,
        Relation::Operator,
    )
    .await
    {
        return response;
    }
    match application::load_tenant(&state, identity.tenant_id.0).await {
        Ok(Some(tenant)) => Json(tenant).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => application_error_response(error),
    }
}

/// Patch tenant account settings.
#[tracing::instrument(skip(state, headers, body))]
pub(crate) async fn patch_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant").await {
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
    let request: PatchTenantRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    match application::patch_tenant(
        &state,
        identity.tenant_id.0,
        request.name.map(|name| name.trim().to_string()),
        request.settings,
    )
    .await
    {
        Ok(true) => get_tenant(State(state), headers).await,
        Ok(false) => (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => application_error_response(error),
    }
}

/// List tenant users.
#[tracing::instrument(skip(state, headers))]
pub(crate) async fn list_users(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant/users").await {
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
    match application::list_users(&state, identity.tenant_id.0).await {
        Ok(users) => Json(serde_json::json!({ "users": users })).into_response(),
        Err(error) => application_error_response(error),
    }
}

/// Create a tenant admin or operator.
#[tracing::instrument(skip(state, headers, body))]
pub(crate) async fn create_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let identity = match authenticate_direct_request(&state, &headers, "/v1/tenant/users").await {
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
    let request: CreateTenantUserRequest = match parse_json_body(&body) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if let Err(response) = validate_password_policy(&request.password) {
        return response;
    }
    if let Err(response) = validate_settings(request.settings.as_ref()) {
        return response;
    }
    let email = normalize_email(&request.email);
    if !looks_like_email(&email) {
        return (StatusCode::BAD_REQUEST, "email must be an email address").into_response();
    }
    match application::create_user(&state, identity.tenant_id.0, request, email).await {
        Ok(user) => (StatusCode::CREATED, Json(user)).into_response(),
        Err(error) => application_error_response(error),
    }
}
