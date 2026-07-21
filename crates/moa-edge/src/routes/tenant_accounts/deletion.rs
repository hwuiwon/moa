//! Durable tenant deletion admission and status routes.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use moa_authz_schema::{ObjectType, Relation};
use moa_wire::tenants::{
    TenantPurgeRequest, TenantPurgeStatus, TenantPurgeStatusRequest, TenantPurgeStatusResponse,
    tenant_id_from_purge_operation_id, tenant_purge_operation_id,
};

use crate::ingress::{IngressScope, call_path, send_path};
use crate::tenant_accounts::{DeleteTenantRequest, application};

use super::super::{
    AppState, authenticate_direct_request, parse_json_body, require_direct_authz, response_to_axum,
};
use super::application_error_response;

/// Authorize and dispatch deletion of this tenant account and tenant-owned data.
#[tracing::instrument(skip(state, headers, body))]
pub(crate) async fn delete_tenant(
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
    let tenant = match application::load_tenant(&state, identity.tenant_id.0).await {
        Ok(Some(tenant)) => tenant,
        Ok(None) => return (StatusCode::NOT_FOUND, "tenant not found").into_response(),
        Err(error) => return application_error_response(error),
    };
    let request = if body.is_empty() {
        DeleteTenantRequest::default()
    } else {
        match parse_json_body::<DeleteTenantRequest>(&body) {
            Ok(request) => request,
            Err(response) => return response,
        }
    };
    if request
        .confirm_slug
        .as_deref()
        .is_some_and(|confirm_slug| confirm_slug != tenant.slug)
    {
        return (
            StatusCode::BAD_REQUEST,
            "confirm_slug does not match tenant",
        )
            .into_response();
    }
    let tenant_id = identity.tenant_id;
    let operation_id = tenant_purge_operation_id(tenant_id);
    let request = TenantPurgeRequest { tenant_id };
    let body = match serde_json::to_vec(&request) {
        Ok(body) => body,
        Err(error) => return super::super::route_error(error),
    };
    let path = send_path(&format!("/TenantPurge/{tenant_id}/run"));
    match state
        .proxy
        .forward(&identity, reqwest::Method::POST, &path, body, &headers)
        .await
    {
        Ok(response) if response.status().is_success() => (
            StatusCode::ACCEPTED,
            Json(TenantPurgeStatusResponse {
                operation_id,
                status: TenantPurgeStatus::Pending,
            }),
        )
            .into_response(),
        Ok(response) => response_to_axum(response).await,
        Err(error) => {
            tracing::error!(error = %error, tenant_id = %tenant_id, "tenant purge dispatch failed");
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

/// Return the durable status of a tenant purge operation.
#[tracing::instrument(skip(state, headers))]
pub(crate) async fn tenant_purge_status(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let identity = match authenticate_direct_request(
        &state,
        &headers,
        "/v1/tenant/purge/{operation_id}",
    )
    .await
    {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let tenant_id = match tenant_id_from_purge_operation_id(&operation_id) {
        Some(tenant_id) => tenant_id,
        None => return (StatusCode::NOT_FOUND, "purge operation not found").into_response(),
    };
    if let Err(response) = authorize_status(&state, &identity, tenant_id).await {
        return response;
    }

    let request = TenantPurgeStatusRequest { tenant_id };
    let body = match serde_json::to_vec(&request) {
        Ok(body) => body,
        Err(error) => return super::super::route_error(error),
    };
    let service_path = format!("/TenantPurge/{tenant_id}/status");
    let path = call_path(&IngressScope::Unscoped, &service_path);
    match state
        .proxy
        .forward(&identity, reqwest::Method::POST, &path, body, &headers)
        .await
    {
        Ok(response) => response_to_axum(response).await,
        Err(error) => {
            tracing::error!(error = %error, operation_id, "tenant purge status forward failed");
            (StatusCode::BAD_GATEWAY, "upstream unavailable").into_response()
        }
    }
}

async fn authorize_status(
    state: &AppState,
    identity: &moa_core::traits::Identity,
    tenant_id: moa_core::types::identifiers::TenantId,
) -> Result<(), Response> {
    if require_direct_authz(
        state,
        identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Admin,
    )
    .await
    .is_ok()
    {
        return Ok(());
    }

    // Tenant-local users, API keys, and tenant-parent tuples are intentionally
    // purged. The canonical workspace admin relation is the only post-delete
    // recovery path; status never becomes unauthenticated or bearer-capability based.
    require_direct_authz(
        state,
        identity,
        ObjectType::Workspace,
        moa_core::WORKSPACE_ID,
        Relation::Admin,
    )
    .await
}
